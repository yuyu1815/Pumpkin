use crate::chunk::format::linear::LinearV2File;
use crate::chunk::format::pump::PumpFile;
use crate::chunk_system::{ChunkListener, ChunkLoading, GenerationSchedule, LevelChannel};
use crate::generation::generator::WorldGenerator;
use crate::lighting::DynamicLightEngine;
use crate::{
    chunk::{
        ChunkData, ChunkEntityData, ChunkReadingError,
        format::anvil::AnvilChunkFile,
        io::{
            Dirtiable, FileIO, LoadedData,
            file_manager::{ChunkFileManager, LevelFileIO},
        },
        palette::has_random_ticking_fluid,
    },
    generation::get_world_gen_with_all_settings,
    tick::{OrderedTick, ScheduledTick, TickPriority},
    world::WorldPortalExt,
};
use arc_swap::ArcSwap;
use crossbeam::queue::SegQueue;
use dashmap::{DashMap, Entry};
use pumpkin_config::{chunk::ChunkConfig, lighting::LightingEngineConfig, world::LevelConfig};
use pumpkin_data::biome::Biome;
use pumpkin_data::dimension::Dimension;
use pumpkin_data::{Block, BlockStateId, block_properties::has_random_ticks, fluid::Fluid};
use pumpkin_util::math::{position::BlockPos, vector2::Vector2};
use pumpkin_util::world_seed::Seed;
use rustc_hash::FxHashSet;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};
// use tokio::runtime::Handle;
use tokio::{
    select,
    sync::{
        Mutex as TokioMutex,
        mpsc::{self, Receiver},
        oneshot,
    },
    task::JoinHandle,
};
use tokio_util::task::TaskTracker;

pub type SyncChunk = Arc<ChunkData>;
pub type SyncEntityChunk = Arc<ChunkEntityData>;

/// The outcome of a bounded entity-chunk read used while persisting live entities.
/// Missing storage is safe to initialize; a read error is not.
pub(crate) enum EntityChunkLoad {
    Loaded(SyncEntityChunk),
    Missing(SyncEntityChunk),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadedChunkChange {
    Loaded(Vector2<i32>),
    Unloaded(Vector2<i32>),
}

pub type ChunkSaver =
    LevelFileIO<LinearV2File<ChunkData>, AnvilChunkFile<ChunkData>, PumpFile<ChunkData>>;

pub type EntitySaver = LevelFileIO<
    LinearV2File<ChunkEntityData>,
    AnvilChunkFile<ChunkEntityData>,
    PumpFile<ChunkEntityData>,
>;

/// The `Level` module provides functionality for working with chunks within or outside a Minecraft world.
///
/// Key features include:
///
/// - **Chunk Loading:** Efficiently loads chunks from disk.
/// - **Chunk Caching:** Stores accessed chunks in memory for faster access.
/// - **Chunk Generation:** Generates new chunks on-demand using a specified `WorldGenerator`.
///
/// For more details on world generation, refer to the `WorldGenerator` module.
pub struct Level {
    pub seed: Seed,
    pub world_portal: ArcSwap<Option<Arc<dyn WorldPortalExt>>>,
    pub level_folder: Arc<LevelFolder>,
    pub lighting_config: LightingEngineConfig,

    /// Counts the number of ticks that have been scheduled for this world
    schedule_tick_counts: AtomicU64,

    // Chunks that are paired with chunk watchers. When a chunk is no longer watched, it is removed
    // from the loaded chunks map and sent to the underlying ChunkIO
    pub loaded_chunks: Arc<DashMap<Vector2<i32>, SyncChunk>>,
    pub(crate) loaded_chunk_changes: Arc<SegQueue<LoadedChunkChange>>,
    loaded_entity_chunks: Arc<DashMap<Vector2<i32>, SyncEntityChunk>>,
    /// Serializes entity snapshot/load/eviction IO so an old async unload write
    /// cannot race a live snapshot loaded for the same nonresident chunk.
    entity_save_lock: Arc<TokioMutex<()>>,
    pub chunks_with_scheduled_ticks: Arc<dashmap::DashSet<Vector2<i32>>>,
    pub chunk_loading: Mutex<ChunkLoading>,

    chunk_watchers: Arc<DashMap<Vector2<i32>, usize>>,

    pub chunk_saver: Arc<ChunkSaver>,
    entity_saver: Arc<EntitySaver>,

    pub world_gen: ArcSwap<WorldGenerator>,

    /// Handles runtime lighting updates
    pub light_engine: DynamicLightEngine,

    /// Tracks tasks associated with this world instance
    tasks: TaskTracker,
    pub chunk_system_tasks: TaskTracker,
    /// Notification that interrupts tasks for shutdown
    pub cancel_token: CancellationToken,

    pub shut_down_chunk_system: AtomicBool,
    pub should_save: AtomicBool,
    pub should_unload: AtomicBool,
    /// Whether periodic autosaving is enabled. Toggled by `/save-off` and `/save-on`;
    /// a manual `/save-all` still saves while this is `false`.
    pub save_enabled: AtomicBool,
    /// Number of ticks between autosave checks. If 0, autosave is disabled.
    pub autosave_ticks: u64,

    pending_entity_generations: Arc<DashMap<Vector2<i32>, Vec<oneshot::Sender<SyncEntityChunk>>>>,

    pub level_channel: Arc<LevelChannel>,
    pub thread_tracker: Mutex<Vec<thread::JoinHandle<()>>>,
    pub chunk_listener: Arc<ChunkListener>,
}

pub struct TickData {
    pub block_ticks: Vec<OrderedTick<&'static Block>>,
    pub fluid_ticks: Vec<OrderedTick<&'static Fluid>>,
    pub random_ticks: Vec<RandomTickSample>,
}

fn merge_tick_streams<T: Clone>(streams: &[Vec<OrderedTick<T>>]) -> Vec<OrderedTick<T>> {
    let mut positions = vec![0; streams.len()];
    let mut heads = BinaryHeap::new();
    for (index, stream) in streams.iter().enumerate() {
        if let Some(tick) = stream.first() {
            heads.push((Reverse(tick.clone()), index));
        }
    }

    let mut merged = Vec::new();
    while let Some((Reverse(_), stream_index)) = heads.pop() {
        let position = positions[stream_index];
        let tick = streams[stream_index][position].clone();
        merged.push(tick);
        positions[stream_index] += 1;
        if let Some(next) = streams[stream_index].get(positions[stream_index]) {
            heads.push((Reverse(next.clone()), stream_index));
        }
    }
    merged
}

#[derive(Clone, Copy)]
pub struct RandomTickSample {
    pub position: BlockPos,
    pub tick_block: bool,
    pub tick_fluid: bool,
}

pub struct LevelFolder {
    pub root_folder: PathBuf,
    pub dim_folder: PathBuf,
    pub region_folder: PathBuf,
    pub entities_folder: PathBuf,
    pub poi_folder: PathBuf,
}

impl Level {
    #[must_use]
    #[expect(clippy::too_many_lines)]
    pub fn from_root_folder(
        level_config: &LevelConfig,
        root_folder: PathBuf,
        seed: i64,
        dimension: Dimension,
    ) -> Arc<Self> {
        let (namespace, name) = match dimension.minecraft_name.split_once(':') {
            Some((ns, n)) => (ns, n),
            None => ("minecraft", dimension.minecraft_name),
        };

        // 26.2 canonical layout: root_folder/dimensions/<namespace>/<name>
        let canonical_dim_folder = root_folder.join("dimensions").join(namespace).join(name);

        // Check if canonical 26.2 folder exists, or fall back to pre-26.2 legacy folders
        let dim_folder = if canonical_dim_folder.exists() {
            canonical_dim_folder
        } else if dimension.minecraft_name == Dimension::OVERWORLD.minecraft_name
            && root_folder.join("region").exists()
        {
            root_folder.clone()
        } else if dimension.minecraft_name == Dimension::THE_NETHER.minecraft_name
            && root_folder.join("DIM-1").join("region").exists()
        {
            root_folder.join("DIM-1")
        } else if dimension.minecraft_name == Dimension::THE_END.minecraft_name
            && root_folder.join("DIM1").join("region").exists()
        {
            root_folder.join("DIM1")
        } else {
            canonical_dim_folder
        };

        let region_folder = dim_folder.join("region");
        let entities_folder = dim_folder.join("entities");
        let poi_folder = dim_folder.join("poi");

        let _ = std::fs::create_dir_all(&region_folder);
        let _ = std::fs::create_dir_all(&entities_folder);
        let _ = std::fs::create_dir_all(&poi_folder);

        let level_folder = Arc::new(LevelFolder {
            root_folder,
            dim_folder,
            region_folder,
            entities_folder,
            poi_folder,
        });

        let main_folder = &level_folder.root_folder;

        let mut is_flat = false;
        let mut flat_layers = Vec::new();
        let mut flat_biome = "minecraft:plains".to_string();
        let mut generator_settings_name: Option<String> = None;
        let mut biome_source: Option<crate::world_info::BiomeSource> = None;
        let mut structure_overrides: Option<Vec<String>> = None;

        if let Some(wgs) = crate::world_info::data_files::read_world_gen_settings(main_folder)
            && let Some(dim_settings) = wgs.dimensions.get(dimension.minecraft_name)
        {
            biome_source.clone_from(&dim_settings.generator.biome_source);

            if dim_settings.generator.generator_type == "minecraft:flat" {
                is_flat = true;
                let flat_settings = dim_settings
                    .generator
                    .settings
                    .as_ref()
                    .and_then(crate::world_info::GeneratorSettings::as_flat_settings)
                    .or_else(|| {
                        crate::world_info::FlatLevelGeneratorPreset::from_name("classic_flat")
                            .map(|p| p.settings)
                    });
                if let Some(flat_settings) = flat_settings {
                    flat_layers = flat_settings.to_flat_layers();
                    structure_overrides = flat_settings.structure_overrides_vec();
                    flat_biome = flat_settings.biome;
                }
            } else if let Some(crate::world_info::GeneratorSettings::Reference(s)) =
                &dim_settings.generator.settings
            {
                generator_settings_name = Some(s.clone());
            }
        }

        let dim_min_y = dimension.min_y;
        let dim_height = dimension.height;
        let seed = Seed(seed as u64);
        let world_gen: Arc<WorldGenerator> = Arc::from(get_world_gen_with_all_settings(
            seed,
            dimension,
            is_flat,
            flat_layers,
            flat_biome,
            generator_settings_name.as_deref(),
            biome_source.as_ref(),
            structure_overrides.as_deref(),
        ));

        let chunk_saver = match &level_config.chunk {
            ChunkConfig::Linear => Arc::new(ChunkSaver::Linear(ChunkFileManager::new(()))),
            ChunkConfig::Anvil(config) => {
                Arc::new(ChunkSaver::Anvil(ChunkFileManager::new(config.clone())))
            }
            ChunkConfig::Pump => Arc::new(ChunkSaver::Pump(ChunkFileManager::new(()))),
        };
        let entity_saver = match &level_config.chunk {
            ChunkConfig::Linear => Arc::new(EntitySaver::Linear(ChunkFileManager::new(()))),
            ChunkConfig::Anvil(config) => {
                Arc::new(EntitySaver::Anvil(ChunkFileManager::new(config.clone())))
            }
            ChunkConfig::Pump => Arc::new(EntitySaver::Pump(ChunkFileManager::new(()))),
        };

        let pending_entity_generations = Arc::new(DashMap::new());
        let level_channel = Arc::new(LevelChannel::new());
        let thread_tracker = Mutex::new(Vec::new());
        let listener = Arc::new(ChunkListener::new());

        let level_ref = Arc::new(Self {
            seed,
            world_portal: ArcSwap::new(Arc::new(None)),
            world_gen: ArcSwap::new(world_gen),
            level_folder,
            lighting_config: level_config.lighting,
            light_engine: DynamicLightEngine::new(dim_min_y, dim_min_y + dim_height),
            chunk_saver,
            entity_saver,
            schedule_tick_counts: AtomicU64::new(0),
            loaded_chunks: Arc::new(DashMap::new()),
            loaded_chunk_changes: Arc::new(SegQueue::new()),
            loaded_entity_chunks: Arc::new(DashMap::new()),
            entity_save_lock: Arc::new(TokioMutex::new(())),
            chunks_with_scheduled_ticks: Arc::new(dashmap::DashSet::new()),
            chunk_loading: Mutex::new(ChunkLoading::new(level_channel.clone())),
            chunk_watchers: Arc::new(DashMap::new()),
            tasks: TaskTracker::new(),
            chunk_system_tasks: TaskTracker::new(),
            cancel_token: CancellationToken::new(),
            shut_down_chunk_system: AtomicBool::new(false),
            should_save: AtomicBool::new(false),
            should_unload: AtomicBool::new(false),
            save_enabled: AtomicBool::new(true),
            autosave_ticks: level_config.autosave_ticks,
            pending_entity_generations,
            level_channel: level_channel.clone(),
            thread_tracker,
            chunk_listener: listener.clone(),
        });

        GenerationSchedule::create(
            4,
            level_ref.clone(),
            level_channel,
            listener,
            level_ref
                .thread_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_mut(),
        );

        level_ref
    }

    pub fn set_world_gen(&self, generator: Arc<WorldGenerator>) {
        self.world_gen.store(generator);
    }

    #[must_use]
    pub fn world_gen(&self) -> Arc<WorldGenerator> {
        self.world_gen.load_full()
    }

    pub fn spawn_entity_generation(self: &Arc<Self>, pos: Vector2<i32>) {
        let level = self.clone();
        rayon::spawn(move || {
            let arc_chunk = Arc::new(ChunkEntityData {
                x: pos.x,
                z: pos.y,
                data: std::sync::Mutex::new(Vec::new()),
                dirty: AtomicBool::new(false),
            });

            // A save may publish a Missing chunk while a normal load is waiting
            // for generation. Never replace that saved snapshot with the empty
            // generation result.
            let arc_chunk = level
                .loaded_entity_chunks
                .entry(pos)
                .or_insert(arc_chunk)
                .clone();

            if let Some((_, waiters)) = level.pending_entity_generations.remove(&pos) {
                for tx in waiters {
                    let _ = tx.send(arc_chunk.clone());
                }
            }
        });
    }

    /// Spawns a task associated with this world. All tasks spawned with this method are awaited
    /// when the client. This means tasks should complete in a reasonable (no looping) amount of time.
    pub fn spawn_task<F>(&self, task: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.tasks.spawn(task)
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        let world_id = self.level_folder.root_folder.display();
        info!("Saving level ({})...", world_id);
        self.cancel_token.cancel();
        self.shut_down_chunk_system.store(true, Ordering::Relaxed);
        self.level_channel.notify();

        self.tasks.close();
        self.chunk_system_tasks.close();

        let handles = {
            let mut lock = self
                .thread_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lock.drain(..).collect::<Vec<_>>()
        };

        let handle_count = handles.len();
        info!("Joining {} threads for {}...", handle_count, world_id);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = std::thread::Builder::new()
            .name("Thread-Joiner".into())
            .spawn(move || {
                let mut failed_count = 0;
                for handle in handles {
                    if handle.join().is_err() {
                        failed_count += 1;
                    }
                }
                let _ = tx.send(failed_count);
            });

        match timeout(Duration::from_secs(3), rx).await {
            Ok(Ok(failed_count)) => {
                if failed_count > 0 {
                    warn!(
                        "{} threads failed to join properly for {}.",
                        failed_count, world_id
                    );
                }
            }
            Ok(Err(_)) => {
                warn!("Thread join task panicked for {}.", world_id);
            }
            Err(_) => {
                warn!("Timed out waiting for threads to join for {}.", world_id);
            }
        }

        self.tasks.wait().await;
        self.chunk_system_tasks.wait().await;

        info!("Flushing chunk data to disk for {}...", world_id);
        self.chunk_saver.block_and_await_ongoing_tasks().await;
        info!("Flushing entity data to disk for {}...", world_id);
        self.entity_saver.block_and_await_ongoing_tasks().await;

        // Serialize final entity-cache removal and the write itself. A save
        // started by a live entity or an eviction must not be able to publish
        // an older snapshot after this final flush.
        let _save_guard = self.entity_save_lock.lock().await;
        let chunks_to_write = self
            .loaded_entity_chunks
            .iter()
            .map(|chunk| (*chunk.key(), chunk.value().clone()))
            .collect::<Vec<_>>();

        // TODO: I think the chunk_saver should be at the server level
        self.entity_saver.clear_watched_chunks().await;
        self.write_entity_chunks(chunks_to_write).await?;
        self.loaded_entity_chunks.clear();
        Ok(())
    }

    pub fn loaded_chunk_count(&self) -> usize {
        self.loaded_chunks.len()
    }

    pub fn list_cached(&self) {
        for entry in self.loaded_chunks.iter() {
            debug!("In map: {:?}", entry.key());
        }
    }

    /// Marks chunks as "watched" by a unique player. When no players are watching a chunk,
    /// it is removed from memory. Should only be called on chunks the player was not watching
    /// before
    pub async fn mark_chunks_as_newly_watched(&self, chunks: &[Vector2<i32>]) {
        for chunk in chunks {
            self.chunk_watchers
                .entry(*chunk)
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1);
        }

        self.entity_saver
            .watch_chunks(&self.level_folder, chunks)
            .await;
    }

    /// Marks chunks no longer "watched" by a unique player. When no players are watching a chunk,
    /// it is removed from memory. Should only be called on chunks the player was watching before
    pub async fn mark_chunks_as_not_watched(
        &self,
        chunks: impl IntoIterator<Item = impl std::borrow::Borrow<Vector2<i32>>>,
    ) -> Vec<Vector2<i32>> {
        let mut chunks_to_clean = Vec::new();
        let chunks_vec: Vec<Vector2<i32>> = chunks.into_iter().map(|c| *c.borrow()).collect();

        for chunk in &chunks_vec {
            if let Entry::Occupied(mut entry) = self.chunk_watchers.entry(*chunk) {
                *entry.get_mut() = entry.get().saturating_sub(1);
                if *entry.get() == 0 {
                    entry.remove();
                    chunks_to_clean.push(*chunk);
                }
            }
        }

        self.entity_saver
            .unwatch_chunks(&self.level_folder, &chunks_vec)
            .await;
        chunks_to_clean
    }

    /// Returns whether the chunk should be removed from memory
    #[inline]
    pub async fn mark_chunk_as_not_watched(&self, chunk: Vector2<i32>) -> bool {
        !self.mark_chunks_as_not_watched([chunk]).await.is_empty()
    }

    // In Level::clean_entity_chunks()
    pub async fn clean_entity_chunks(
        self: &Arc<Self>,
        chunks: impl IntoIterator<Item = impl std::borrow::Borrow<Vector2<i32>>>,
    ) -> Result<(), String> {
        // The map removal must be covered by the same lock as load/merge/write.
        // Removing it before waiting allowed a concurrent live save to load a
        // disk snapshot and then be overwritten by this older in-memory chunk.
        let _save_guard = self.entity_save_lock.lock().await;
        let chunks_to_process: Vec<_> = chunks
            .into_iter()
            .filter_map(|pos_borrow| {
                let pos = pos_borrow.borrow();
                let has_watchers = self
                    .chunk_watchers
                    .get(pos)
                    .is_some_and(|count| *count != 0);

                if has_watchers {
                    return None;
                }

                self.loaded_entity_chunks.remove(pos)
            })
            .collect();

        if chunks_to_process.is_empty() {
            return Ok(());
        }

        debug!("Writing {} entity chunks to disk", chunks_to_process.len());
        if let Err(error) = self.write_entity_chunks(chunks_to_process.clone()).await {
            // Keep the cache and its entity data available for a retry. The
            // caller must not treat a failed write as a completed eviction.
            for (pos, chunk) in chunks_to_process {
                self.loaded_entity_chunks.entry(pos).or_insert(chunk);
            }
            return Err(error);
        }
        Ok(())
    }
    pub fn get_tick_data(
        &self,
        active_chunks: &FxHashSet<Vector2<i32>>,
        random_tick_speed: i64,
    ) -> TickData {
        let samples_per_section = random_tick_speed.max(0);

        let mut ticks = TickData {
            block_ticks: Vec::new(),
            fluid_ticks: Vec::new(),
            random_ticks: Vec::with_capacity(active_chunks.len() * 3),
        };
        let mut block_tick_streams = Vec::new();
        let mut fluid_tick_streams = Vec::new();

        // 1. Process active chunks (random ticks, block entities)
        for pos in active_chunks {
            if let Some(chunk) = self.loaded_chunks.get(pos) {
                let chunk = chunk.value();
                let chunk_x_base = chunk.x * 16;
                let chunk_z_base = chunk.z * 16;
                let section_count = chunk.section.count;

                // Use the bitmask to skip sections
                let mask = chunk.section.randomly_ticking_mask.load(Ordering::Relaxed);
                if mask != 0 {
                    let sections = chunk
                        .section
                        .block_sections
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let min_y = chunk.section.min_y;

                    for i in 0..section_count {
                        if (mask & (1 << i)) == 0 {
                            continue;
                        }
                        let y_base = min_y + (i as i32 * 16);
                        for _ in 0..samples_per_section {
                            let r = rand::random::<u32>();
                            let x_offset = (r & 0xF) as usize;
                            let z_offset = (r >> 8 & 0xF) as usize;
                            let y_in_section = ((r >> 4) & 0xF) as usize;

                            let block_state_id = sections[i].get(x_offset, y_in_section, z_offset);
                            let tick_block = has_random_ticks(block_state_id);
                            let tick_fluid = has_random_ticking_fluid(block_state_id);
                            if tick_block || tick_fluid {
                                ticks.random_ticks.push(RandomTickSample {
                                    position: BlockPos::new(
                                        chunk_x_base + x_offset as i32,
                                        y_base + y_in_section as i32,
                                        chunk_z_base + z_offset as i32,
                                    ),
                                    tick_block,
                                    tick_fluid,
                                });
                            }
                        }
                    }
                }
            }
        }

        // 2. Process chunks with scheduled ticks
        // We collect keys first to avoid holding DashSet shard lock while accessing loaded_chunks (deadlock risk)
        let scheduled_chunk_pos: Vec<_> = self
            .chunks_with_scheduled_ticks
            .iter()
            .map(|p| *p)
            .collect();
        for pos in scheduled_chunk_pos {
            if let Some(chunk) = self.loaded_chunks.get(&pos) {
                let chunk = chunk.value();
                let active = active_chunks.contains(&pos);
                chunk.block_ticks.step_tick();
                chunk.fluid_ticks.step_tick();
                // Tick countdown changes the serialized scheduler state even when no callback runs.
                chunk.mark_dirty(true);
                if active {
                    let block_ticks = chunk.block_ticks.take_ready_ticks();
                    if !block_ticks.is_empty() {
                        block_tick_streams.push(block_ticks);
                    }
                    let fluid_ticks = chunk.fluid_ticks.take_ready_ticks();
                    if !fluid_ticks.is_empty() {
                        fluid_tick_streams.push(fluid_ticks);
                    }
                }

                // Remove from set if it no longer has ticks
                if !chunk.block_ticks.has_ticks() && !chunk.fluid_ticks.has_ticks() {
                    self.chunks_with_scheduled_ticks.remove(&pos);
                }
            } else {
                self.chunks_with_scheduled_ticks.remove(&pos); // Chunk unloaded
            }
        }

        ticks.block_ticks = merge_tick_streams(&block_tick_streams);
        ticks.fluid_ticks = merge_tick_streams(&fluid_tick_streams);

        ticks
    }

    pub async fn clean_entity_chunk(self: &Arc<Self>, chunk: &Vector2<i32>) -> Result<(), String> {
        self.clean_entity_chunks([*chunk]).await
    }

    pub fn is_chunk_watched(&self, chunk: &Vector2<i32>) -> bool {
        self.chunk_watchers.get(chunk).is_some()
    }

    pub fn clean_memory(self: &Arc<Self>) -> Vec<Vector2<i32>> {
        self.chunk_watchers.retain(|_, watcher| *watcher != 0);

        let entity_chunks_to_remove: Vec<_> = self
            .loaded_entity_chunks
            .iter()
            .filter(|entry| !self.chunk_watchers.contains_key(entry.key()))
            .map(|entry| *entry.key())
            .collect();

        // We do not clean them here because we want the caller to save any active entities in them first.

        // if the difference is too big, we can shrink the loaded chunks
        // (1024 chunks is the equivalent to a 32x32 chunks area)
        if self.chunk_watchers.capacity() - self.chunk_watchers.len() >= 4096 {
            self.chunk_watchers.shrink_to_fit();
        }

        if self.loaded_chunks.capacity() - self.loaded_chunks.len() >= 4096 {
            self.loaded_chunks.shrink_to_fit();
        }

        if self.loaded_entity_chunks.capacity() - self.loaded_entity_chunks.len() >= 4096 {
            self.loaded_entity_chunks.shrink_to_fit();
        }
        entity_chunks_to_remove
    }

    fn register_scheduled_ticks(&self, pos: Vector2<i32>, chunk: &ChunkData) {
        if chunk.block_ticks.has_ticks() || chunk.fluid_ticks.has_ticks() {
            self.chunks_with_scheduled_ticks.insert(pos);
        }
    }

    pub async fn get_or_fetch_chunk<R, F: Fn(&SyncChunk) -> R>(
        self: &Arc<Self>,
        pos: Vector2<i32>,
        f: F,
    ) -> R {
        // Check if already in memory
        if let Some(res) = self.read_chunk_sync(&pos, |chunk| {
            self.register_scheduled_ticks(pos, chunk);
            f(chunk)
        }) {
            return res;
        }
        let chunk = self.fetch_chunk(pos).await;
        if self.loaded_chunks.insert(pos, chunk.clone()).is_none() {
            self.loaded_chunk_changes
                .push(LoadedChunkChange::Loaded(pos));
        }
        if let Some(loaded) = self.loaded_chunks.get(&pos) {
            self.register_scheduled_ticks(pos, loaded.value());
        }
        f(&chunk)
    }

    pub fn loaded_chunk_changes(&self) -> impl Iterator<Item = LoadedChunkChange> + '_ {
        std::iter::from_fn(|| self.loaded_chunk_changes.pop())
    }

    async fn fetch_chunk(self: &Arc<Self>, pos: Vector2<i32>) -> SyncChunk {
        let recv = self.chunk_listener.add_single_chunk_listener(pos);

        {
            let mut lock = self
                .chunk_loading
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lock.add_ticket(pos, ChunkLoading::FULL_CHUNK_LEVEL);
            lock.send_change();
        };

        let chunk = recv
            .await
            .unwrap_or_else(|_| ChunkData::empty_sync(pos.x, pos.y));

        {
            let mut lock = self
                .chunk_loading
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lock.remove_ticket(pos, ChunkLoading::FULL_CHUNK_LEVEL);
            lock.send_change();
        };

        chunk
    }

    async fn load_single_entity_chunk(
        &self,
        pos: Vector2<i32>,
    ) -> Result<(SyncEntityChunk, bool), ChunkReadingError> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        self.entity_saver
            .fetch_chunks(&self.level_folder, &[pos], tx)
            .await;

        match rx.recv().await {
            Some(LoadedData::Loaded(chunk)) => Ok((chunk, false)),
            Some(LoadedData::Error((_, err))) => Err(err),
            _ => Err(ChunkReadingError::ChunkNotExist),
        }
    }

    /// Reads one entity chunk without falling back to entity generation.
    ///
    /// This path is deliberately separate from `get_entity_chunk`: shutdown,
    /// autosave, and eviction must never turn a malformed/I/O-failed file into
    /// an empty chunk and then overwrite the original bytes.
    async fn load_entity_chunk_for_save_unlocked(
        &self,
        pos: Vector2<i32>,
    ) -> Result<EntityChunkLoad, String> {
        if let Some(chunk) = self.loaded_entity_chunks.get(&pos) {
            return Ok(EntityChunkLoad::Loaded(chunk.value().clone()));
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        self.entity_saver
            .fetch_chunks(&self.level_folder, &[pos], tx)
            .await;

        match rx.recv().await {
            Some(LoadedData::Loaded(chunk)) => Ok(EntityChunkLoad::Loaded(chunk)),
            Some(LoadedData::Missing(_)) => {
                Ok(EntityChunkLoad::Missing(Arc::new(ChunkEntityData {
                    x: pos.x,
                    z: pos.y,
                    data: std::sync::Mutex::new(Vec::new()),
                    dirty: AtomicBool::new(false),
                })))
            }
            Some(LoadedData::Error((_, error))) => {
                Err(format!("failed to load entity chunk {pos:?}: {error}"))
            }
            None => Err(format!(
                "entity chunk loader closed without a result for {pos:?}"
            )),
        }
    }

    #[cfg(test)]
    pub(crate) async fn load_entity_chunk_for_save(
        &self,
        pos: Vector2<i32>,
    ) -> Result<EntityChunkLoad, String> {
        let _save_guard = self.entity_save_lock.lock().await;
        self.load_entity_chunk_for_save_unlocked(pos).await
    }

    /// Serializes and persists one live entity under the same lock as entity
    /// eviction. This keeps load, merge, and write atomic relative to an old
    /// chunk-clean task and makes repeated saves replace the same UUID instead
    /// of duplicating it.
    pub async fn save_entity_nbt(
        &self,
        pos: Vector2<i32>,
        entity_uuid: uuid::Uuid,
        nbt: pumpkin_nbt::compound::NbtCompound,
    ) -> Result<(), String> {
        self.save_entity_nbt_with_stale_chunk(pos, entity_uuid, nbt, None)
            .await
    }

    /// Saves a live entity and optionally removes its previous disk snapshot.
    /// The stale chunk is loaded even when it is not resident, closing the
    /// boundary where a moved entity could otherwise retain an old NBT record.
    pub async fn save_entity_nbt_with_stale_chunk(
        &self,
        pos: Vector2<i32>,
        entity_uuid: uuid::Uuid,
        nbt: pumpkin_nbt::compound::NbtCompound,
        stale_pos: Option<Vector2<i32>>,
    ) -> Result<(), String> {
        let _save_guard = self.entity_save_lock.lock().await;
        let chunk = match self.load_entity_chunk_for_save_unlocked(pos).await? {
            EntityChunkLoad::Loaded(chunk) => chunk,
            EntityChunkLoad::Missing(chunk) => self
                .loaded_entity_chunks
                .entry(pos)
                .or_insert(chunk)
                .clone(),
        };
        {
            let mut data = chunk
                .data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut live_nbt = Some(nbt);
            data.retain_mut(|existing| {
                if existing.get_uuid("UUID") != Some(entity_uuid) {
                    return true;
                }
                live_nbt.take().is_some_and(|replacement| {
                    *existing = replacement;
                    true
                })
            });
            if let Some(nbt) = live_nbt {
                data.push(nbt);
            }
        }
        chunk.mark_dirty(true);

        // A live entity can have been saved once in its old chunk before it
        // crosses a boundary. Remove that stale snapshot from every other
        // cached entity chunk before publishing the new destination snapshot;
        // otherwise reopen would resurrect a duplicate at the old position.
        let mut chunks_to_write = vec![(pos, chunk)];
        let cached_chunks = self
            .loaded_entity_chunks
            .iter()
            .filter(|entry| *entry.key() != pos)
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect::<Vec<_>>();
        for (cached_pos, cached_chunk) in cached_chunks {
            let removed = {
                let mut data = cached_chunk
                    .data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let original_len = data.len();
                data.retain(|existing| existing.get_uuid("UUID") != Some(entity_uuid));
                data.len() != original_len
            };
            if removed {
                cached_chunk.mark_dirty(true);
                chunks_to_write.push((cached_pos, cached_chunk));
            }
        }

        if let Some(stale_pos) = stale_pos
            && stale_pos != pos
            && self.loaded_entity_chunks.get(&stale_pos).is_none()
            && let EntityChunkLoad::Loaded(stale_chunk) =
                self.load_entity_chunk_for_save_unlocked(stale_pos).await?
        {
            let removed = {
                let mut data = stale_chunk
                    .data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let original_len = data.len();
                data.retain(|existing| existing.get_uuid("UUID") != Some(entity_uuid));
                data.len() != original_len
            };
            if removed {
                stale_chunk.mark_dirty(true);
                chunks_to_write.push((stale_pos, stale_chunk));
            }
        }

        self.entity_saver
            .save_chunks(&self.level_folder, chunks_to_write)
            .await
            .map_err(|error| format!("failed to persist entity chunk {pos:?}: {error}"))
    }

    /// Persists one entity chunk and surfaces serializer/I/O failures to the
    /// caller.
    #[cfg(test)]
    pub(crate) async fn persist_entity_chunk(
        &self,
        pos: Vector2<i32>,
        chunk: SyncEntityChunk,
    ) -> Result<(), String> {
        let _save_guard = self.entity_save_lock.lock().await;
        self.entity_saver
            .save_chunks(&self.level_folder, vec![(pos, chunk)])
            .await
            .map_err(|error| format!("failed to persist entity chunk {pos:?}: {error}"))
    }

    pub fn receive_entity_chunks(
        self: &Arc<Self>,
        chunks: Vec<Vector2<i32>>,
    ) -> Receiver<(Weak<ChunkEntityData>, bool)> {
        let (sender, receiver) = mpsc::channel(64);
        let level = self.clone();

        self.spawn_task(async move {
            let cancel_notifier = level.cancel_token.cancelled();

            let fetch_task = async {
                let to_fetch: Vec<_> = chunks
                    .iter()
                    .filter(|pos| {
                        level.loaded_entity_chunks.get(pos).is_none_or(|chunk| {
                            let _ = sender.try_send((Arc::downgrade(chunk.value()), false));
                            false // Don't fetch
                        })
                    })
                    .copied()
                    .collect();

                if !to_fetch.is_empty() {
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<
                        LoadedData<SyncEntityChunk, ChunkReadingError>,
                    >(to_fetch.len());

                    // Linearize the disk read with insertion into the cache. A
                    // live save cannot otherwise race this fetch and have its
                    // newer file replaced by the fetched old snapshot.
                    let save_guard = level.entity_save_lock.lock().await;
                    level
                        .entity_saver
                        .fetch_chunks(&level.level_folder, &to_fetch, tx)
                        .await;

                    let mut loaded_notifications = Vec::new();
                    let mut generation_waiters = Vec::new();
                    while let Some(data) = rx.recv().await {
                        match data {
                            LoadedData::Loaded(chunk) => {
                                let pos = Vector2::new(chunk.x, chunk.z);
                                level.loaded_entity_chunks.insert(pos, chunk.clone());
                                loaded_notifications.push(chunk);
                            }
                            LoadedData::Missing(pos) => {
                                let (tx, rx) = oneshot::channel();
                                match level.pending_entity_generations.entry(pos) {
                                    dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                                        entry.get_mut().push(tx);
                                    }
                                    dashmap::mapref::entry::Entry::Vacant(entry) => {
                                        entry.insert(vec![tx]);
                                        level.spawn_entity_generation(pos);
                                    }
                                }
                                generation_waiters.push(rx);
                            }
                            LoadedData::Error((pos, error)) => {
                                // A corrupt/unreadable entity file is not a missing
                                // chunk. Do not generate an empty replacement: that
                                // would make the next save destroy the original bytes.
                                error!("Failed to load entity chunk {pos:?}: {error}");
                            }
                        }
                    }
                    drop(save_guard);

                    for chunk in loaded_notifications {
                        let _ = sender.send((Arc::downgrade(&chunk), true)).await;
                    }
                    for rx in generation_waiters {
                        let sender_clone = sender.clone();
                        tokio::spawn(async move {
                            if let Ok(chunk) = rx.await {
                                let _ = sender_clone.send((Arc::downgrade(&chunk), true)).await;
                            }
                        });
                    }
                }
            };

            select! {
                () = cancel_notifier => {},
                () = fetch_task => {}
            }
        });

        receiver
    }

    pub async fn get_entity_chunk(self: &Arc<Self>, pos: Vector2<i32>) -> SyncEntityChunk {
        if let Some(chunk) = self.loaded_entity_chunks.get(&pos) {
            return chunk.clone();
        }

        // Keep ordinary entity loads in the same ordering domain as save and
        // eviction. The generation waiter is registered before releasing the
        // guard; its async result is awaited afterwards.
        let save_guard = self.entity_save_lock.lock().await;
        if let Some(chunk) = self.loaded_entity_chunks.get(&pos) {
            return chunk.clone();
        }

        if let Ok((chunk, _)) = self.load_single_entity_chunk(pos).await {
            self.loaded_entity_chunks.insert(pos, chunk.clone());
            chunk
        } else {
            let (tx, rx) = oneshot::channel();
            match self.pending_entity_generations.entry(pos) {
                dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                    entry.get_mut().push(tx);
                }
                dashmap::mapref::entry::Entry::Vacant(entry) => {
                    entry.insert(vec![tx]);
                    self.spawn_entity_generation(pos);
                }
            }
            drop(save_guard);
            rx.await.unwrap_or_else(|_| {
                Arc::new(ChunkEntityData {
                    x: pos.x,
                    z: pos.y,
                    data: std::sync::Mutex::new(Vec::new()),
                    dirty: AtomicBool::new(false),
                })
            })
        }
    }

    pub fn get_block_state(&self, position: &BlockPos) -> BlockStateId {
        let (chunk_coordinate, relative) = position.chunk_and_chunk_relative_position();
        let id = self
            .read_chunk_sync(&chunk_coordinate, |chunk| {
                chunk.section.get_block_absolute_y(
                    relative.x as usize,
                    relative.y,
                    relative.z as usize,
                )
            })
            .flatten();

        id.unwrap_or(Block::VOID_AIR.default_state.id)
    }

    pub fn set_block_state(
        &self,
        position: &BlockPos,
        block_state_id: BlockStateId,
    ) -> BlockStateId {
        let (chunk_coordinate, relative) = position.chunk_and_chunk_relative_position();
        self.read_chunk_sync(&chunk_coordinate, |chunk| {
            let replaced_block_state_id = chunk.set_block_absolute_y(
                relative.x as usize,
                relative.y,
                relative.z as usize,
                block_state_id,
            );
            if replaced_block_state_id != block_state_id {
                chunk.mark_dirty(true);
            }
            replaced_block_state_id
        })
        .unwrap_or(Block::VOID_AIR.default_state.id)
    }

    pub async fn write_chunks(&self, chunks_to_write: Vec<(Vector2<i32>, SyncChunk)>) {
        if chunks_to_write.is_empty() {
            return;
        }

        let chunk_saver = self.chunk_saver.clone();
        let level_folder = self.level_folder.clone();

        trace!("Sending chunks to ChunkIO {:}", chunks_to_write.len());
        if let Err(error) = chunk_saver
            .save_chunks(&level_folder, chunks_to_write)
            .await
        {
            error!("Failed writing Chunk to disk {error}");
        }
    }

    async fn write_entity_chunks(
        &self,
        chunks_to_write: Vec<(Vector2<i32>, SyncEntityChunk)>,
    ) -> Result<(), String> {
        if chunks_to_write.is_empty() {
            return Ok(());
        }

        let chunk_saver = self.entity_saver.clone();
        let level_folder = self.level_folder.clone();

        trace!("Sending chunks to ChunkIO {:}", chunks_to_write.len());
        chunk_saver
            .save_chunks(&level_folder, chunks_to_write)
            .await
            .map_err(|error| format!("failed writing entity chunks to disk: {error}"))
    }

    pub fn is_chunk_loaded(&self, coordinates: &Vector2<i32>) -> bool {
        self.loaded_chunks.contains_key(coordinates)
    }

    pub fn read_chunk_sync<R, F: Fn(&SyncChunk) -> R>(
        &self,
        coordinates: &Vector2<i32>,
        f: F,
    ) -> Option<R> {
        self.loaded_chunks.get(coordinates).map(|x| f(x.value()))
    }

    pub fn read_entity_chunk_sync<R, F: Fn(&SyncEntityChunk) -> R>(
        &self,
        coordinates: &Vector2<i32>,
        f: F,
    ) -> Option<R> {
        self.loaded_entity_chunks
            .get(coordinates)
            .map(|x| f(x.value()))
    }

    pub fn get_rough_biome(&self, position: &BlockPos) -> &'static Biome {
        let (chunk_coordinate, relative) = position.chunk_and_chunk_relative_position();
        let id = self.read_chunk_sync(&chunk_coordinate, |chunk| {
            chunk.section.get_rough_biome_absolute_y(
                relative.x as usize,
                relative.y,
                relative.z as usize,
            )
        });
        Biome::from_id(id.flatten().unwrap_or(0)).unwrap_or(&Biome::THE_VOID)
    }

    pub fn get_entity_chunk_sync(&self, pos: &Vector2<i32>) -> Option<SyncEntityChunk> {
        self.loaded_entity_chunks
            .get(pos)
            .map(|x| x.value().clone())
    }

    pub async fn get_or_fetch_entity_chunk<R, F: Fn(&SyncEntityChunk) -> R>(
        self: &Arc<Self>,
        pos: Vector2<i32>,
        f: F,
    ) -> R {
        if let Some(res) = self.read_entity_chunk_sync(&pos, &f) {
            return res;
        }
        let chunk = self.get_entity_chunk(pos).await;
        f(&chunk)
    }

    pub fn try_get_entity_chunk(
        &self,
        coordinates: Vector2<i32>,
    ) -> Option<dashmap::mapref::one::Ref<'_, Vector2<i32>, Arc<ChunkEntityData>>> {
        self.loaded_entity_chunks.try_get(&coordinates).try_unwrap()
    }

    pub fn schedule_block_tick(
        &self,
        block: &Block,
        block_pos: BlockPos,
        delay: u8,
        priority: TickPriority,
    ) {
        let tick_order = self.schedule_tick_counts.fetch_add(1, Ordering::Relaxed);
        let scheduled_tick = ScheduledTick {
            delay: i32::from(delay),
            position: block_pos,
            priority,
            // SAFETY: `block` is a valid reference that outlives this function call for scheduling.
            value: unsafe { &*std::ptr::from_ref::<Block>(block) },
        };

        let chunk_pos = block_pos.chunk_position();
        if self
            .read_chunk_sync(&chunk_pos, |chunk| {
                chunk.block_ticks.schedule_tick(&scheduled_tick, tick_order);
            })
            .is_some()
        {
            self.chunks_with_scheduled_ticks.insert(chunk_pos);
        }
    }

    pub fn schedule_fluid_tick(
        &self,
        fluid: &Fluid,
        block_pos: BlockPos,
        delay: u8,
        priority: TickPriority,
    ) {
        let tick_order = self.schedule_tick_counts.fetch_add(1, Ordering::Relaxed);
        let scheduled_tick = ScheduledTick {
            delay: i32::from(delay),
            position: block_pos,
            priority,
            // SAFETY: `fluid` is a valid reference that outlives this function call for scheduling.
            value: unsafe { &*std::ptr::from_ref::<Fluid>(fluid) },
        };

        let chunk_pos = block_pos.chunk_position();
        if self
            .read_chunk_sync(&chunk_pos, |chunk| {
                chunk.fluid_ticks.schedule_tick(&scheduled_tick, tick_order);
            })
            .is_some()
        {
            self.chunks_with_scheduled_ticks.insert(chunk_pos);
        }
    }

    pub fn is_block_tick_scheduled(&self, block_pos: &BlockPos, block: &Block) -> bool {
        self.read_chunk_sync(&block_pos.chunk_position(), |chunk| {
            chunk.block_ticks.is_scheduled(*block_pos, block)
        })
        .unwrap_or(false)
    }

    pub fn is_fluid_tick_scheduled(&self, block_pos: &BlockPos, fluid: &Fluid) -> bool {
        self.read_chunk_sync(&block_pos.chunk_position(), |chunk| {
            chunk.fluid_ticks.is_scheduled(*block_pos, fluid)
        })
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_config::world::LevelConfig;
    use pumpkin_nbt::compound::NbtCompound;
    use tempfile::TempDir;

    #[tokio::test]
    async fn dimension_paths_26_2() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().to_path_buf();
        let config = LevelConfig::default();

        let overworld_level =
            Level::from_root_folder(&config, root.clone(), 0, Dimension::OVERWORLD);
        assert_eq!(
            overworld_level.level_folder.dim_folder,
            root.join("dimensions").join("minecraft").join("overworld")
        );
        assert_eq!(
            overworld_level.level_folder.region_folder,
            root.join("dimensions")
                .join("minecraft")
                .join("overworld")
                .join("region")
        );

        let nether_level = Level::from_root_folder(&config, root.clone(), 0, Dimension::THE_NETHER);
        assert_eq!(
            nether_level.level_folder.dim_folder,
            root.join("dimensions").join("minecraft").join("the_nether")
        );

        let end_level = Level::from_root_folder(&config, root.clone(), 0, Dimension::THE_END);
        assert_eq!(
            end_level.level_folder.dim_folder,
            root.join("dimensions").join("minecraft").join("the_end")
        );
    }

    #[tokio::test]
    async fn cross_chunk_local_deadlines_do_not_override_priority() {
        let temp_dir = TempDir::new().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            temp_dir.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );
        let old_chunk = Vector2::new(0, 0);
        let new_chunk = Vector2::new(1, 0);
        let old_target = BlockPos::new(0, 64, 0);
        let old_future = BlockPos::new(1, 64, 0);
        let new_target = BlockPos::new(16, 64, 0);
        let old_fluid_target = BlockPos::new(2, 64, 0);
        let new_fluid_target = BlockPos::new(18, 64, 0);
        level
            .loaded_chunks
            .insert(old_chunk, ChunkData::empty_sync(old_chunk.x, old_chunk.y));
        level
            .loaded_chunks
            .insert(new_chunk, ChunkData::empty_sync(new_chunk.x, new_chunk.y));

        // Age only the old chunk before scheduling both due ticks.
        level.schedule_block_tick(&Block::STONE, old_future, 255, TickPriority::Normal);
        let inactive_chunks = FxHashSet::from_iter([Vector2::new(99, 0)]);
        for _ in 0..100 {
            level.get_tick_data(&inactive_chunks, 0);
        }
        level.schedule_block_tick(&Block::STONE, old_target, 0, TickPriority::High);
        level.schedule_block_tick(&Block::DIRT, new_target, 0, TickPriority::Normal);
        level.schedule_fluid_tick(&Fluid::WATER, old_fluid_target, 0, TickPriority::High);
        level.schedule_fluid_tick(&Fluid::WATER, new_fluid_target, 0, TickPriority::Normal);

        let active_chunks = FxHashSet::from_iter([old_chunk, new_chunk]);
        let ticks = level.get_tick_data(&active_chunks, 0);
        let actual: Vec<_> = ticks
            .block_ticks
            .into_iter()
            .map(|tick| tick.position)
            .collect();
        assert_eq!(actual, vec![old_target, new_target]);
        let actual: Vec<_> = ticks
            .fluid_ticks
            .into_iter()
            .map(|tick| tick.position)
            .collect();
        assert_eq!(actual, vec![old_fluid_target, new_fluid_target]);
        level.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn inactive_overdue_ticks_keep_deadline_order() {
        let temp_dir = TempDir::new().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            temp_dir.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );
        let chunk_pos = Vector2::new(3, 0);
        let late_deadline = BlockPos::new(48, 64, 0);
        let early_deadline = BlockPos::new(49, 64, 0);
        let late_fluid_deadline = BlockPos::new(50, 64, 0);
        let early_fluid_deadline = BlockPos::new(51, 64, 0);
        level
            .loaded_chunks
            .insert(chunk_pos, ChunkData::empty_sync(chunk_pos.x, chunk_pos.y));

        // The later deadline is higher priority; each chunk must still drain by deadline.
        level.schedule_block_tick(&Block::STONE, late_deadline, 3, TickPriority::High);
        level.schedule_block_tick(&Block::STONE, early_deadline, 1, TickPriority::Normal);
        level.schedule_fluid_tick(&Fluid::WATER, late_fluid_deadline, 3, TickPriority::High);
        level.schedule_fluid_tick(&Fluid::WATER, early_fluid_deadline, 1, TickPriority::Normal);

        let inactive_chunks = FxHashSet::from_iter([Vector2::new(0, 0)]);
        for _ in 0..5 {
            let ticks = level.get_tick_data(&inactive_chunks, 0);
            assert!(ticks.block_ticks.is_empty());
            assert!(ticks.fluid_ticks.is_empty());
        }

        let active_chunks = FxHashSet::from_iter([chunk_pos]);
        let ticks = level.get_tick_data(&active_chunks, 0);
        let actual: Vec<_> = ticks
            .block_ticks
            .into_iter()
            .map(|tick| tick.position)
            .collect();
        assert_eq!(actual, vec![early_deadline, late_deadline]);
        let actual: Vec<_> = ticks
            .fluid_ticks
            .into_iter()
            .map(|tick| tick.position)
            .collect();
        assert_eq!(actual, vec![early_fluid_deadline, late_fluid_deadline]);
        level.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn scheduled_ticks_keep_absolute_deadlines_outside_active_chunks() {
        let temp_dir = TempDir::new().unwrap();
        let config = LevelConfig::default();
        let level = Level::from_root_folder(
            &config,
            temp_dir.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );
        let chunk_pos = Vector2::new(3, 0);
        let block_due = BlockPos::new(48, 64, 0);
        let block_future = BlockPos::new(49, 64, 0);
        let fluid_due = BlockPos::new(50, 64, 0);
        let fluid_future = BlockPos::new(51, 64, 0);
        level
            .loaded_chunks
            .insert(chunk_pos, ChunkData::empty_sync(chunk_pos.x, chunk_pos.y));
        level.schedule_block_tick(&Block::STONE, block_due, 2, TickPriority::Normal);
        level.schedule_block_tick(&Block::STONE, block_future, 8, TickPriority::Normal);
        level.schedule_fluid_tick(&Fluid::WATER, fluid_due, 2, TickPriority::Normal);
        level.schedule_fluid_tick(&Fluid::WATER, fluid_future, 9, TickPriority::Normal);

        let inactive_chunks = FxHashSet::from_iter([Vector2::new(0, 0)]);
        for _ in 0..5 {
            let ticks = level.get_tick_data(&inactive_chunks, 0);
            assert!(ticks.block_ticks.is_empty());
            assert!(ticks.fluid_ticks.is_empty());
        }
        // A new deadline for the same block/value is still deduplicated while inactive.
        level.schedule_block_tick(&Block::STONE, block_due, 200, TickPriority::Low);
        assert!(level.is_block_tick_scheduled(&block_due, &Block::STONE));
        assert!(level.is_fluid_tick_scheduled(&fluid_due, &Fluid::WATER));
        assert!(level.loaded_chunks.get(&chunk_pos).unwrap().is_dirty());
        assert_eq!(
            level
                .loaded_chunks
                .get(&chunk_pos)
                .unwrap()
                .block_ticks
                .to_vec()
                .iter()
                .filter(|tick| tick.position == block_due)
                .count(),
            1
        );
        assert_eq!(
            level
                .loaded_chunks
                .get(&chunk_pos)
                .unwrap()
                .block_ticks
                .to_vec()
                .iter()
                .find(|tick| tick.position == block_due)
                .map(|tick| tick.delay),
            Some(-3)
        );
        assert_eq!(
            level
                .loaded_chunks
                .get(&chunk_pos)
                .unwrap()
                .block_ticks
                .to_vec()
                .iter()
                .find(|tick| tick.position == block_future)
                .map(|tick| tick.delay),
            Some(3)
        );
        assert_eq!(
            level
                .loaded_chunks
                .get(&chunk_pos)
                .unwrap()
                .fluid_ticks
                .to_vec()
                .iter()
                .find(|tick| tick.position == fluid_due)
                .map(|tick| tick.delay),
            Some(-3)
        );
        assert_eq!(
            level
                .loaded_chunks
                .get(&chunk_pos)
                .unwrap()
                .fluid_ticks
                .to_vec()
                .iter()
                .find(|tick| tick.position == fluid_future)
                .map(|tick| tick.delay),
            Some(4)
        );

        let active_chunks = FxHashSet::from_iter([chunk_pos]);
        let ticks = level.get_tick_data(&active_chunks, 0);
        assert_eq!(ticks.block_ticks.len(), 1);
        assert_eq!(ticks.block_ticks[0].position, block_due);
        assert_eq!(ticks.fluid_ticks.len(), 1);
        assert_eq!(ticks.fluid_ticks[0].position, fluid_due);

        assert!(
            level
                .get_tick_data(&active_chunks, 0)
                .block_ticks
                .is_empty()
        );
        assert!(
            level
                .get_tick_data(&active_chunks, 0)
                .block_ticks
                .is_empty()
        );
        let ticks = level.get_tick_data(&active_chunks, 0);
        assert_eq!(ticks.block_ticks.len(), 1);
        assert_eq!(ticks.block_ticks[0].position, block_future);
        let ticks = level.get_tick_data(&active_chunks, 0);
        assert_eq!(ticks.fluid_ticks.len(), 1);
        assert_eq!(ticks.fluid_ticks[0].position, fluid_future);

        let unloaded_chunk = Vector2::new(4, 0);
        let unloaded_pos = BlockPos::new(64, 64, 0);
        level.loaded_chunks.insert(
            unloaded_chunk,
            ChunkData::empty_sync(unloaded_chunk.x, unloaded_chunk.y),
        );
        level.schedule_block_tick(&Block::STONE, unloaded_pos, 0, TickPriority::Normal);
        level.loaded_chunks.remove(&unloaded_chunk);
        assert!(level.chunks_with_scheduled_ticks.contains(&unloaded_chunk));
        level.get_tick_data(&active_chunks, 0);
        assert!(!level.chunks_with_scheduled_ticks.contains(&unloaded_chunk));
        level.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn loaded_saved_ticks_are_registered_for_processing() {
        let temp_dir = TempDir::new().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            temp_dir.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );
        let chunk_pos = Vector2::new(2, 0);
        let tick_pos = BlockPos::new(32, 64, 0);
        level
            .loaded_chunks
            .insert(chunk_pos, ChunkData::empty_sync(chunk_pos.x, chunk_pos.y));
        level.schedule_block_tick(&Block::STONE, tick_pos, 0, TickPriority::Normal);

        // Simulate a chunk loaded with a saved-only tick before the registry is rebuilt.
        level.chunks_with_scheduled_ticks.remove(&chunk_pos);
        level.get_or_fetch_chunk(chunk_pos, |_| ()).await;
        let active_chunks = FxHashSet::from_iter([chunk_pos]);
        let ticks = level.get_tick_data(&active_chunks, 0);
        assert_eq!(
            ticks
                .block_ticks
                .iter()
                .map(|tick| tick.position)
                .collect::<Vec<_>>(),
            vec![tick_pos]
        );
        level.shutdown().await.unwrap();
    }

    #[test]
    fn clear_area_removes_ready_and_future_ticks() {
        let scheduler = crate::tick::scheduler::ChunkTickScheduler::default();
        let value: &'static Block = unsafe { &*std::ptr::from_ref(&Block::STONE) };
        let ready_pos = BlockPos::new(0, 64, 0);
        let future_pos = BlockPos::new(2, 64, 0);
        scheduler.schedule_tick(
            &ScheduledTick {
                delay: 0,
                priority: TickPriority::Normal,
                position: ready_pos,
                value,
            },
            0,
        );
        scheduler.schedule_tick(
            &ScheduledTick {
                delay: 10,
                priority: TickPriority::Normal,
                position: future_pos,
                value,
            },
            1,
        );
        scheduler.step_tick();
        scheduler.clear_area(&BlockPos::new(-1, 0, -1), &BlockPos::new(1, 256, 1));
        assert!(!scheduler.is_scheduled(ready_pos, value));
        assert!(scheduler.is_scheduled(future_pos, value));
        scheduler.clear_area(&BlockPos::new(-1, 0, -1), &BlockPos::new(3, 256, 1));
        assert!(!scheduler.has_ticks());
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn entity_save_load_reopen_preserves_live_and_existing_nbt() {
        let temp_dir = TempDir::new().unwrap();
        let config = LevelConfig::default();
        let pos = Vector2::new(7, -3);
        let first_uuid = uuid::Uuid::from_u128(1);
        let second_uuid = uuid::Uuid::from_u128(2);

        let level = Level::from_root_folder(
            &config,
            temp_dir.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );
        let EntityChunkLoad::Missing(chunk) = level
            .load_entity_chunk_for_save(pos)
            .await
            .expect("fresh entity chunk must be Missing")
        else {
            panic!("fresh entity chunk was unexpectedly Loaded");
        };
        let mut first = NbtCompound::new();
        first.put_string("id", "minecraft:item".to_string());
        first.put_uuid("UUID", first_uuid);
        first.put_list(
            "Pos",
            vec![
                pumpkin_nbt::tag::NbtTag::Double(112.5),
                pumpkin_nbt::tag::NbtTag::Double(64.0),
                pumpkin_nbt::tag::NbtTag::Double(-47.25),
            ],
        );
        first.put_string("TestMarker", "first".to_string());
        chunk
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(first);
        chunk.mark_dirty(true);
        level
            .persist_entity_chunk(pos, chunk)
            .await
            .expect("missing entity chunk save must succeed");

        let EntityChunkLoad::Loaded(chunk) = level
            .load_entity_chunk_for_save(pos)
            .await
            .expect("saved entity chunk must load")
        else {
            panic!("saved entity chunk was unexpectedly Missing");
        };
        let mut second = NbtCompound::new();
        second.put_string("id", "minecraft:armor_stand".to_string());
        second.put_uuid("UUID", second_uuid);
        second.put_list(
            "Pos",
            vec![
                pumpkin_nbt::tag::NbtTag::Double(113.5),
                pumpkin_nbt::tag::NbtTag::Double(65.0),
                pumpkin_nbt::tag::NbtTag::Double(-46.25),
            ],
        );
        second.put_string("TestMarker", "live-snapshot".to_string());
        chunk
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(second);
        chunk.mark_dirty(true);
        level
            .persist_entity_chunk(pos, chunk)
            .await
            .expect("live snapshot save must succeed");
        level.shutdown().await.expect("fixture level shutdown");

        let reopened = Level::from_root_folder(
            &config,
            temp_dir.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );
        let EntityChunkLoad::Loaded(chunk) = reopened
            .load_entity_chunk_for_save(pos)
            .await
            .expect("reopened entity chunk must load")
        else {
            panic!("reopened entity chunk was unexpectedly Missing");
        };
        {
            let entities = chunk
                .data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(entities.len(), 2);
            assert!(entities.iter().any(|entity| {
                entity.get_uuid("UUID") == Some(first_uuid)
                    && entity.get_string("id") == Some("minecraft:item")
                    && entity.get_string("TestMarker") == Some("first")
            }));
            assert!(entities.iter().any(|entity| {
                entity.get_uuid("UUID") == Some(second_uuid)
                    && entity.get_string("id") == Some("minecraft:armor_stand")
                    && entity.get_string("TestMarker") == Some("live-snapshot")
            }));
        };
        reopened.shutdown().await.expect("reopened level shutdown");
    }

    #[tokio::test]
    async fn entity_save_load_error_does_not_overwrite_raw_region_bytes() {
        let temp_dir = TempDir::new().unwrap();
        let config = LevelConfig::default();
        let level = Level::from_root_folder(
            &config,
            temp_dir.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );
        let pos = Vector2::new(0, 0);
        let chunk = Arc::new(ChunkEntityData {
            x: pos.x,
            z: pos.y,
            data: std::sync::Mutex::new(Vec::new()),
            dirty: AtomicBool::new(true),
        });
        level
            .persist_entity_chunk(pos, chunk)
            .await
            .expect("fixture entity region write must succeed");
        level.shutdown().await.expect("fixture level shutdown");

        let mut entity_files = Vec::new();
        let mut pending = vec![level.level_folder.entities_folder.clone()];
        while let Some(path) = pending.pop() {
            for entry in std::fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    entity_files.push(path);
                }
            }
        }
        assert_eq!(entity_files.len(), 1);
        let corrupt_bytes = b"malformed entity region bytes".to_vec();
        std::fs::write(&entity_files[0], &corrupt_bytes).unwrap();

        let reopened = Level::from_root_folder(
            &config,
            temp_dir.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );
        let result = reopened.load_entity_chunk_for_save(pos).await;
        assert!(result.is_err(), "malformed entity IO must fail closed");
        assert_eq!(std::fs::read(&entity_files[0]).unwrap(), corrupt_bytes);
        reopened.shutdown().await.expect("reopened level shutdown");
    }

    #[tokio::test]
    async fn generated_missing_chunk_does_not_replace_saved_snapshot() {
        let temp_dir = TempDir::new().unwrap();
        let config = LevelConfig::default();
        let pos = Vector2::new(2, 4);
        let entity_uuid = uuid::Uuid::from_u128(3);
        let level = Level::from_root_folder(
            &config,
            temp_dir.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );

        let mut nbt = NbtCompound::new();
        nbt.put_string("id", "minecraft:item".to_string());
        nbt.put_uuid("UUID", entity_uuid);
        nbt.put_string("TestMarker", "saved-before-generation".to_string());
        level
            .save_entity_nbt(pos, entity_uuid, nbt)
            .await
            .expect("save missing chunk snapshot");

        let (tx, rx) = oneshot::channel();
        level.pending_entity_generations.insert(pos, vec![tx]);
        level.spawn_entity_generation(pos);
        let generated = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("entity generation completion")
            .expect("entity generation waiter");
        {
            let entities = generated
                .data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(entities.len(), 1);
            assert_eq!(
                entities[0].get_string("TestMarker"),
                Some("saved-before-generation")
            );
        };
        level.shutdown().await.expect("test level shutdown");
    }

    #[tokio::test]
    async fn disk_only_stale_entity_snapshot_is_removed_on_boundary_save() {
        let temp_dir = TempDir::new().unwrap();
        let config = LevelConfig::default();
        let old_pos = Vector2::new(0, 0);
        let new_pos = Vector2::new(2, 0);
        let entity_uuid = uuid::Uuid::from_u128(4);
        let level = Level::from_root_folder(
            &config,
            temp_dir.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );

        let mut old_nbt = NbtCompound::new();
        old_nbt.put_string("id", "minecraft:item".to_string());
        old_nbt.put_uuid("UUID", entity_uuid);
        old_nbt.put_string("TestMarker", "old".to_string());
        level
            .save_entity_nbt(old_pos, entity_uuid, old_nbt)
            .await
            .expect("old snapshot save");
        level
            .clean_entity_chunks([old_pos])
            .await
            .expect("old chunk eviction");

        let mut new_nbt = NbtCompound::new();
        new_nbt.put_string("id", "minecraft:item".to_string());
        new_nbt.put_uuid("UUID", entity_uuid);
        new_nbt.put_string("TestMarker", "new".to_string());
        level
            .save_entity_nbt_with_stale_chunk(new_pos, entity_uuid, new_nbt, Some(old_pos))
            .await
            .expect("boundary save with disk-only stale chunk");
        level.shutdown().await.expect("test level shutdown");

        let reopened = Level::from_root_folder(
            &config,
            temp_dir.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );
        let EntityChunkLoad::Loaded(old_chunk) = reopened
            .load_entity_chunk_for_save(old_pos)
            .await
            .expect("old chunk reopen")
        else {
            panic!("old chunk missing after stale cleanup");
        };
        assert!(
            old_chunk
                .data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
        let EntityChunkLoad::Loaded(new_chunk) = reopened
            .load_entity_chunk_for_save(new_pos)
            .await
            .expect("new chunk reopen")
        else {
            panic!("new chunk missing after boundary save");
        };
        assert_eq!(
            new_chunk
                .data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|entity| entity.get_uuid("UUID") == Some(entity_uuid))
                .count(),
            1
        );
        reopened.shutdown().await.expect("reopened level shutdown");
    }

    #[tokio::test]
    async fn legacy_dimension_fallback() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().to_path_buf();
        let config = LevelConfig::default();

        // Create legacy directories
        std::fs::create_dir_all(root.join("region")).unwrap();
        std::fs::create_dir_all(root.join("DIM-1").join("region")).unwrap();
        std::fs::create_dir_all(root.join("DIM1").join("region")).unwrap();

        let overworld_level =
            Level::from_root_folder(&config, root.clone(), 0, Dimension::OVERWORLD);
        assert_eq!(overworld_level.level_folder.dim_folder, root);

        let nether_level = Level::from_root_folder(&config, root.clone(), 0, Dimension::THE_NETHER);
        assert_eq!(nether_level.level_folder.dim_folder, root.join("DIM-1"));

        let end_level = Level::from_root_folder(&config, root.clone(), 0, Dimension::THE_END);
        assert_eq!(end_level.level_folder.dim_folder, root.join("DIM1"));
    }
}
