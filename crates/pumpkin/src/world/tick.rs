use super::{
    World, dragon_fight,
    natural_spawner::{self, SpawnState, spawn_for_chunk},
};
use crate::block::entities::BlockEntity;
use crate::block::{OnScheduledTickArgs, RandomTickArgs};
use crate::entity::{Entity, EntityBase};
use crate::server::Server;
use pumpkin_data::Block;
use pumpkin_data::entity::{EntityType, MobCategory};
use pumpkin_util::Difficulty;
use pumpkin_util::math::{get_section_cord, vector2::Vector2, vector3::Vector3};
use pumpkin_world::chunk::{ChunkData, ChunkHeightmapType::MotionBlocking};
use rand::seq::SliceRandom;
use rand::{RngExt, rng};
use rayon::prelude::*;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use tracing::{debug, error};

const SCHEDULED_TICK_BATCH_SIZE: usize = 32;

fn dispatch_scheduled_ticks<T, F>(ticks: &[T], callback: F)
where
    T: Sync,
    F: Fn(&[T]) + Send + Sync,
{
    ticks.chunks(SCHEDULED_TICK_BATCH_SIZE).for_each(callback);
}

impl World {
    #[expect(clippy::too_many_lines)]
    pub fn tick(self: &Arc<Self>, server: &Arc<Server>) {
        const ENTITY_TICK_BATCH_SIZE: usize = 16;

        let start = std::time::Instant::now();

        self.flush_block_updates();
        self.flush_synced_block_events();
        self.update_active_chunks();
        self.tick_environment();
        let mut raids = {
            let mut guard = self
                .raids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *guard)
        };
        raids.tick(self);
        {
            let mut guard = self
                .raids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (id, raid) in guard.raid_map.drain() {
                raids.raid_map.insert(id, raid);
            }
            raids.next_id = raids.next_id.max(guard.next_id);
            *guard = raids;
        };

        let t_chunks = std::time::Instant::now();
        self.tick_chunks(server);
        let chunk_elapsed = t_chunks.elapsed();

        let handle = server.runtime.clone();

        let players = self.players.load();
        let player_count = players.len();
        let players_cache: Vec<_> = players
            .par_iter()
            .map(|player| {
                let entity = player.get_entity();
                let pos = entity.pos.load();
                let bb = entity.bounding_box.load().expand(1.0, 0.5, 1.0);
                let chunk_pos = Vector2::new(
                    get_section_cord(pos.x.floor() as i32),
                    get_section_cord(pos.z.floor() as i32),
                );
                (player, pos, bb, chunk_pos)
            })
            .collect();

        let t_players = std::time::Instant::now();
        let player_handle = handle.clone();
        players.par_iter().for_each(|player| {
            let _guard = player_handle.enter();
            player.tick(server);
        });
        let player_elapsed = t_players.elapsed();

        let entities_to_tick = self.entities.load();
        let entity_count = entities_to_tick.len();
        let active_chunks = self.active_chunks.snapshot();
        let level_for_entities = self.level.clone();
        let entity_handle = handle.clone();

        let t_entities = std::time::Instant::now();
        let tickable: Vec<_> = entities_to_tick
            .par_iter()
            .filter_map(|entity| {
                let entity_pos = entity.get_entity().pos.load();
                let entity_chunk = Vector2::new(
                    get_section_cord(entity_pos.x.floor() as i32),
                    get_section_cord(entity_pos.z.floor() as i32),
                );
                if !active_chunks.contains(&entity_chunk) {
                    return None;
                }
                if !level_for_entities.is_chunk_loaded(&entity_chunk) {
                    return None;
                }
                Some((entity, entity_chunk))
            })
            .collect();

        let server_ref = server.as_ref();
        tickable
            .par_chunks(ENTITY_TICK_BATCH_SIZE)
            .for_each(|batch| {
                let _guard = entity_handle.enter();

                for (entity, entity_chunk) in batch {
                    entity.get_entity().age.fetch_add(1, Relaxed);
                    entity.tick(entity.as_ref(), server_ref);

                    let entity_inner = entity.get_entity();
                    let entity_pos = entity_inner.pos.load();
                    let entity_bb = entity_inner.bounding_box.load();

                    for (player, player_pos, player_bb, player_chunk) in &players_cache {
                        if (player_chunk.x - entity_chunk.x).abs() <= 1
                            && (player_chunk.y - entity_chunk.y).abs() <= 1
                            && (player_pos.x - entity_pos.x).abs() < 5.0
                            && (player_pos.y - entity_pos.y).abs() < 5.0
                            && (player_pos.z - entity_pos.z).abs() < 5.0
                            && player_bb.intersects(&entity_bb)
                        {
                            entity.on_player_collision(player);
                            break;
                        }
                    }
                }
            });
        let entity_elapsed = t_entities.elapsed();

        self.entity_tracker.update_all(self);

        let mut block_entities: Vec<Arc<dyn BlockEntity>> = Vec::new();
        if self.block_entities.len() < active_chunks.len() {
            for chunk_block_entities in &self.block_entities {
                if active_chunks.contains(chunk_block_entities.key()) {
                    block_entities.extend(chunk_block_entities.values().cloned());
                }
            }
        } else {
            for chunk_pos in active_chunks.iter() {
                if let Some(chunk_block_entities) = self.block_entities.get(chunk_pos) {
                    block_entities.extend(chunk_block_entities.values().cloned());
                }
            }
        }
        let block_entity_count = block_entities.len();

        let t_be = std::time::Instant::now();
        let be_handle = handle;
        block_entities.par_chunks(16).for_each(|batch| {
            let _guard = be_handle.enter();
            for be in batch {
                be.tick(self);
            }
        });
        // Drained after all ticks, so changes (hopper -> chest) land in the same tick.
        let guard = be_handle.enter();
        self.flush_comparator_updates(&block_entities);
        drop(guard);
        let block_entity_elapsed = t_be.elapsed();

        self.level
            .chunk_loading
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send_change();

        if let Some(ref fight_mutex) = self.dragon_fight {
            dragon_fight::DragonFight::tick(fight_mutex, self);
        }

        let total_elapsed = start.elapsed();
        if total_elapsed.as_millis() > 50 {
            debug!(
                "Slow Tick [{}ms]: Chunks: {:?} | Players({}): {:?} | Entities({}): {:?} | Block Entities({}): {:?}",
                total_elapsed.as_millis(),
                chunk_elapsed,
                player_count,
                player_elapsed,
                entity_count,
                entity_elapsed,
                block_entity_count,
                block_entity_elapsed,
            );
        }
    }

    fn tick_environment(self: &Arc<Self>) {
        let (world_age, is_night, time_of_day) = {
            let mut level_time = self
                .level_time
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let advance_time = self.level_info.load().game_rules.advance_time;
            level_time.tick(advance_time);

            // Auto-save logic
            if level_time.world_age % 100 == 0 {
                self.level.should_unload.store(true, Relaxed);
                let cleaned_chunks = self.level.clean_memory();
                if !cleaned_chunks.is_empty() {
                    let world_clone = self.clone();
                    if let Some(server) = self.server.upgrade() {
                        server.spawn_task(async move {
                            if let Err(error) =
                                world_clone.remove_entities_in_chunks(&cleaned_chunks).await
                            {
                                error!("Autosave entity eviction failed: {error}");
                                return;
                            }
                            if let Err(error) =
                                world_clone.level.clean_entity_chunks(&cleaned_chunks).await
                            {
                                error!("Autosave entity chunk cleanup failed: {error}");
                            }
                        });
                    }
                }
                // If autosave is configured and this tick will trigger an autosave, don't double notify
                if self.level.autosave_ticks == 0 {
                    self.level.level_channel.notify();
                } else {
                    let autosave = self.level.autosave_ticks as i64;
                    if autosave == 0 || level_time.world_age % autosave != 0 {
                        self.level.level_channel.notify();
                    }
                }
            }
            if self.level.autosave_ticks > 0 && self.level.save_enabled.load(Relaxed) {
                let autosave = self.level.autosave_ticks as i64;
                if autosave > 0 && level_time.world_age % autosave == 0 {
                    self.level.should_save.store(true, Relaxed);
                    self.level.level_channel.notify();
                }
            }
            (
                level_time.world_age,
                level_time.is_night(),
                level_time.time_of_day,
            )
        };

        let (should_reset_weather, weather_cycle_enabled) = {
            let mut weather = self
                .weather
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            weather.tick_weather(self);
            (
                weather.raining || weather.thundering,
                weather.weather_cycle_enabled,
            )
        };

        if self.should_skip_night() && is_night {
            let level_time = {
                let mut guard = self
                    .level_time
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let time = time_of_day + 24000;
                guard.set_time(time - time % 24000);
                guard.clone()
            };
            level_time.send_time(self);

            for player in self.players.load().iter() {
                player.wake_up();
            }

            if weather_cycle_enabled && should_reset_weather {
                let mut weather = self
                    .weather
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                weather.reset_weather_cycle(self);
            }
        } else if world_age % 20 == 0 {
            let level_time = self
                .level_time
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            level_time.send_time(self);
        }
    }

    #[expect(clippy::too_many_lines)]
    fn tick_chunks(self: &Arc<Self>, server: &Arc<Server>) {
        const BATCH_SIZE: usize = SCHEDULED_TICK_BATCH_SIZE;
        const INHABITED_TIME_BATCH_SIZE: usize = 1024;
        let random_tick_speed = self.level_info.load().game_rules.random_tick_speed;

        let active_chunks = self.active_chunks.snapshot();
        let tick_data = self.level.get_tick_data(&active_chunks, random_tick_speed);
        let handle = server.runtime.clone();

        // 1. Ordered Block Ticks. Scheduled callbacks may mutate shared world state, so their
        // input order is part of the tick contract.
        let world = self.clone();
        let block_handle = handle.clone();
        dispatch_scheduled_ticks(&tick_data.block_ticks, |batch| {
            let _guard = block_handle.enter();
            let world = world.clone();
            for scheduled_tick in batch {
                let pos = scheduled_tick.position;
                let block = world.get_block(&pos);
                if let Some(pumpkin_block) = world.block_registry.get_pumpkin_block(block.id) {
                    pumpkin_block.on_scheduled_tick(OnScheduledTickArgs {
                        world: &world,
                        block,
                        position: &pos,
                    });
                }
            }
        });

        // 2. Ordered Fluid Ticks. Keep the block phase before the fluid phase.
        let world = self.clone();
        let fluid_handle = handle.clone();
        dispatch_scheduled_ticks(&tick_data.fluid_ticks, |batch| {
            let _guard = fluid_handle.enter();
            let world = world.clone();
            for scheduled_tick in batch {
                let pos = scheduled_tick.position;
                let fluid = world.get_fluid(&pos);
                if let Some(pumpkin_fluid) = world.block_registry.get_pumpkin_fluid(fluid.id) {
                    pumpkin_fluid.on_scheduled_tick(&world, fluid, &pos);
                }
            }
        });

        // 3. Parallel Random Ticks via Rayon
        let world = self.clone();
        let random_handle = handle.clone();
        tick_data
            .random_ticks
            .par_chunks(BATCH_SIZE)
            .for_each(|batch| {
                let _guard = random_handle.enter();
                let world = world.clone();
                for scheduled_tick in batch {
                    let pos = scheduled_tick.position;
                    let (block, fluid) =
                        match (scheduled_tick.tick_block, scheduled_tick.tick_fluid) {
                            (true, true) => {
                                let (b, f) = world.get_block_and_fluid(&pos);
                                (Some(b), Some(f))
                            }
                            (true, false) => (Some(world.get_block(&pos)), None),
                            (false, true) => (None, Some(world.get_fluid(&pos))),
                            (false, false) => (None, None),
                        };

                    if let Some(block) = block
                        && let Some(pumpkin_block) =
                            world.block_registry.get_pumpkin_block(block.id)
                    {
                        pumpkin_block.random_tick(RandomTickArgs {
                            world: &world,
                            block,
                            position: &pos,
                        });
                    }

                    if let Some(fluid) = fluid
                        && let Some(pumpkin_fluid) =
                            world.block_registry.get_pumpkin_fluid(fluid.id)
                    {
                        pumpkin_fluid.random_tick(fluid, &world, &pos);
                    }
                }
            });

        // 4. Calculate Spawn List (Sequential setup)
        let spawn_state = self.spawn_state.load();
        let (spawn_mobs, spawn_monsters, peaceful) = {
            let lock = self.level_info.load();
            (
                lock.game_rules.spawn_mobs,
                lock.game_rules.spawn_monsters,
                lock.difficulty == Difficulty::Peaceful,
            )
        };
        let spawn_passives = self.get_time_of_day() % 400 == 0;
        let spawn_enemies = !peaceful && spawn_monsters && spawn_mobs;
        let spawn_passives = spawn_passives && spawn_mobs;

        let spawn_list = Arc::new(natural_spawner::get_filtered_spawning_categories(
            &spawn_state,
            spawn_mobs,
            spawn_enemies,
            spawn_passives,
        ));

        // 5. Parallel Chunk Spawners via Rayon
        if !spawn_list.is_empty() {
            let mut spawning_chunks = Vec::new();
            for pos in active_chunks.iter() {
                if let Some(chunk) = self.level.read_chunk_sync(pos, std::clone::Clone::clone) {
                    spawning_chunks.push((*pos, chunk));
                }
            }

            spawning_chunks.shuffle(&mut rng());

            let world = self.clone();
            let spawn_handle = handle;
            spawning_chunks.par_chunks(8).for_each(|batch| {
                let _guard = spawn_handle.enter();
                let world = world.clone();
                let s_list = spawn_list.clone();
                let s_state = spawn_state.clone();
                for (pos, chunk) in batch {
                    world.tick_spawning_chunk(*pos, chunk, &s_list, &s_state);
                }
            });
        }

        // Batch these cheap lookups and atomic increments to avoid waking Rayon
        // workers for tiny tasks every tick, while retaining parallelism for large sets.
        let loaded_chunks = self.level.loaded_chunks.clone();
        let active_chunks_vec: Vec<_> = active_chunks.iter().copied().collect();
        active_chunks_vec
            .par_iter()
            .with_min_len(INHABITED_TIME_BATCH_SIZE)
            .for_each(|pos| {
                if let Some(chunk) = loaded_chunks.get(pos) {
                    chunk.inhabited_time.fetch_add(1, Relaxed);
                }
            });
    }

    fn tick_spawning_chunk(
        self: &Arc<Self>,
        chunk_pos: Vector2<i32>,
        chunk: &Arc<ChunkData>,
        spawn_list: &Vec<&'static MobCategory>,
        spawn_state: &Arc<SpawnState>,
    ) {
        // this.level.tickThunder(chunk);
        //TODO check in simulation distance
        let (is_raining, is_thundering) = (self.is_raining(), self.is_thundering());

        if is_raining && is_thundering && rng().random_range(0..100_000) == 0 {
            let rand_value = rng().random::<i32>() >> 2;
            let delta = Vector3::new(rand_value & 15, rand_value >> 16 & 15, rand_value >> 8 & 15);
            let random_pos = Vector3::new(
                chunk_pos.x << 4,
                chunk
                    .heightmap
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(
                        MotionBlocking,
                        chunk_pos.x << 4,
                        chunk_pos.y << 4,
                        self.min_y,
                    ),
                chunk_pos.y << 4,
            )
            .add(&delta);
            // TODO this.getBrightness(LightLayer.SKY, blockPos) >= 15;
            // TODO heightmap

            // TODO findLightningRod(blockPos)
            // TODO encapsulatingFullBlocks
            if true {
                // TODO biome.getPrecipitationAt(pos, this.getSeaLevel()) == Biome.Precipitation.RAIN
                // TODO this.getCurrentDifficultyAt(blockPos);
                if rng().random::<f32>() < 0.0675
                    && self.get_block(&random_pos.to_block_pos().down()) != &Block::LIGHTNING_ROD
                {
                    let entity = Entity::new(
                        self.clone(),
                        random_pos.to_f64(),
                        &EntityType::SKELETON_HORSE,
                    );
                    self.spawn_entity_non_save(Arc::new(entity));
                }
                let entity = Entity::new(
                    self.clone(),
                    random_pos.to_f64().add_raw(0.5, 0., 0.5),
                    &EntityType::LIGHTNING_BOLT,
                );
                self.spawn_entity_non_save(Arc::new(entity));
            }
        }

        if spawn_list.is_empty() {
            return;
        }
        // TODO this.level.canSpawnEntitiesInChunk(chunkPos)
        let entities = spawn_for_chunk(
            self,
            chunk_pos,
            chunk,
            spawn_state,
            spawn_list,
            is_thundering,
        );
        for entity in entities {
            self.spawn_entity_non_save(entity);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::dispatch_scheduled_ticks;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    #[test]
    fn scheduled_tick_dispatch_preserves_global_order_across_33_callbacks() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("test rayon pool");
        let head_recorded = Arc::new((Mutex::new(false), Condvar::new()));
        let trace = Arc::new(Mutex::new(Vec::new()));
        let ticks: Vec<u32> = (0..33).collect();

        pool.install(|| {
            dispatch_scheduled_ticks(&ticks, |batch| {
                if batch[0] == 32 {
                    trace.lock().unwrap().extend_from_slice(batch);
                    let mut recorded = head_recorded.0.lock().unwrap();
                    *recorded = true;
                    head_recorded.1.notify_one();
                } else {
                    let recorded = head_recorded.0.lock().unwrap();
                    let _ = head_recorded
                        .1
                        .wait_timeout_while(recorded, Duration::from_millis(100), |seen| !*seen)
                        .unwrap();
                    trace.lock().unwrap().extend_from_slice(batch);
                }
            });
        });

        assert_eq!(*trace.lock().unwrap(), (0..33).collect::<Vec<_>>());
    }
}
