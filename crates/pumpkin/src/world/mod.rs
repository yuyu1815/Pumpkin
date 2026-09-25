use crate::block::entities::BlockEntity;
use dashmap::DashMap;
use pumpkin_data::chunk::Biome;
use pumpkin_protocol::bedrock::client::block_actor_data::CBlockActorData;
use pumpkin_protocol::bedrock::client::level_event::{CLevelEvent, LevelEvent};
use pumpkin_world::generation::proto_chunk::GenerationCache;
use std::sync::{Arc, Weak};
use std::{collections::HashMap, sync::atomic::Ordering};
use tracing::{debug, trace, warn};

pub(crate) mod active_chunks;

#[derive(Clone, Copy)]
pub struct SculkEventSource {
    pub uuid: uuid::Uuid,
    pub projectile_owner: Option<uuid::Uuid>,
    pub spectator: bool,
    pub sneaking: bool,
    pub dampens_vibrations: bool,
}

fn catalyst_experience_charge(eligible: bool, reward: u32) -> Option<i32> {
    eligible
        .then(|| i32::try_from(reward).ok())
        .flatten()
        .filter(|charge| *charge > 0)
}

fn prefer_catalyst(current: Option<(f64, BlockPos)>, candidate: (f64, BlockPos)) -> bool {
    current.is_none_or(|current| {
        candidate.0.total_cmp(&current.0).is_lt()
            || (candidate.0 == current.0
                && (candidate.1.0.x, candidate.1.0.y, candidate.1.0.z)
                    < (current.1.0.x, current.1.0.y, current.1.0.z))
    })
}

fn sculk_event_listeners(event: pumpkin_data::game_event::GameEvent) -> (bool, bool) {
    use pumpkin_data::tag::{RegistryKey, get_tag_values};
    (
        get_tag_values(RegistryKey::GameEvent, "minecraft:vibrations")
            .is_some_and(|events| events.contains(&event.name())),
        get_tag_values(RegistryKey::GameEvent, "minecraft:shrieker_can_listen")
            .is_some_and(|events| events.contains(&event.name())),
    )
}

fn shrieker_source_eligible(
    projectile_owner: bool,
    direct_player: bool,
    item_owner: bool,
    passenger: bool,
) -> bool {
    projectile_owner || direct_player || item_owner || passenger
}

fn within_sculk_listener_radius(event: BlockPos, listener: BlockPos, radius: i32) -> bool {
    let dx = listener.0.x - event.0.x;
    let dy = listener.0.y - event.0.y;
    let dz = listener.0.z - event.0.z;
    dx * dx + dy * dy + dz * dz <= radius * radius
}
mod block_access;
mod block_entity;
mod broadcast;
pub mod chunker;
pub mod explosion;
pub mod generation_cache;
pub mod loot;
pub mod map;
mod player_lifecycle;
mod player_membership;
mod player_spawn;
pub mod portal;
pub mod raid;
pub mod random_sequences;
pub mod stopwatches;
mod tick;
pub mod time;
pub mod villager_poi;

use crate::{block::BlockEvent, entity::item::ItemEntity};
use crate::{
    block::{OnNeighborUpdateArgs, registry::BlockRegistry},
    entity::{Entity, EntityBase, RemovalReason, player::Player, r#type::from_type},
    error::PumpkinError,
    net::ClientPlatform,
    plugin::block::block_break::BlockBreakEvent,
    server::Server,
};
use active_chunks::ActivePlayerArea;
use arc_swap::ArcSwap;
use border::Worldborder;
pub use explosion::{
    BlockInteraction, DefaultExplosionDamageCalculator, Explosion, ExplosionDamageCalculator,
    ExplosionInteraction, SimpleExplosionDamageCalculator,
};
use player_membership::PlayerMembership;
use pumpkin_data::block_properties::is_air;
use pumpkin_data::block_rotation::{Mirror, Rotation};
use pumpkin_data::dimension::Dimension;
use pumpkin_data::game_rules::{GameRule, GameRuleValue};
use pumpkin_data::noise_settings::NoiseSettings;
use pumpkin_data::{
    Block, BlockStateId, entity::EntityType, fluid::Fluid, item_stack::ItemStack,
    particle::Particle, sound::Sound, sound_id_remap::remap_sound_id_for_version,
    world::WorldEvent,
};
use pumpkin_data::{
    BlockDirection, BlockState,
    block_properties::{ChestLikeProperties, ChestType},
    tag::Taggable,
};
use pumpkin_inventory::Inventory;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_protocol::java::client::play::CBlockEvent;
use pumpkin_protocol::java::client::play::{
    CBlockUpdate, CExplosion, CSetBlockDestroyStage, CWorldEvent,
};
use pumpkin_protocol::{
    IdOr, SoundEvent,
    bedrock::client::block_event::CBlockEvent as CBedrockBlockEvent,
    codec::var_int::VarInt,
    java::client::play::{CBlockEntityData, CMultiBlockUpdate},
};
use pumpkin_util::text::TextComponent;
use pumpkin_util::{
    Difficulty,
    math::{boundingbox::BoundingBox, position::BlockPos, vector3::Vector3},
};
use pumpkin_util::{
    math::{position::chunk_section_from_pos, vector2::Vector2},
    random::{RandomImpl, get_seed, xoroshiro128::Xoroshiro},
};
use pumpkin_world::chunk::{io::Dirtiable, palette::bedrock_water_state};
use pumpkin_world::world::BlockAccessor;
use pumpkin_world::world::{GetBlockError, WorldPortalExt};
use pumpkin_world::{level::Level, tick::TickPriority};
pub use pumpkin_world::{world::BlockFlags, world_info::LevelData};
use rand::RngExt;
use scoreboard::Scoreboard;
use time::LevelTime;

pub mod block_placer;
pub mod border;
pub mod bossbar;
pub mod custom_bossbar;
pub mod dragon_fight;
pub mod end_podium;
pub mod entity_tracker;
pub mod environment;
pub mod natural_spawner;
pub mod scoreboard;
pub mod weather;

pub use environment::EnvironmentAttributes;
pub use pumpkin_data::environment_attribute::{Activity, MoonPhase};

use crate::world::natural_spawner::SpawnState;
use pumpkin_config::lighting::LightingEngineConfig;
use pumpkin_world::chunk::ChunkHeightmapType::MotionBlocking;
use uuid::Uuid;
use weather::Weather;

const MAX_LIGHT_LEVEL: u8 = 15;

fn bedrock_chest_block_actor(state_id: BlockStateId, position: BlockPos) -> Option<NbtCompound> {
    let (block, _) = BlockState::from_id_with_block(state_id);
    if !block.has_tag(&pumpkin_data::tag::Block::C_CHESTS_WOODEN)
        && !block.has_tag(&pumpkin_data::tag::Block::MINECRAFT_COPPER_CHESTS)
    {
        return None;
    }

    // Block actor tags describe the chest itself. Container contents are synchronized
    // through inventory packets and must not be exposed in chunk data.
    let mut nbt = NbtCompound::new();
    nbt.put_string("id", "Chest".to_string());
    nbt.put_int("x", position.0.x);
    nbt.put_int("y", position.0.y);
    nbt.put_int("z", position.0.z);
    nbt.put_bool("isMovable", true);

    let properties = ChestLikeProperties::from_state_id(state_id);
    if properties.r#type != ChestType::Single {
        let direction = if properties.r#type == ChestType::Left {
            properties.facing.rotate_clockwise()
        } else {
            properties.facing.rotate_counter_clockwise()
        };
        let pair = position.offset(direction.to_offset());
        nbt.put_int("pairx", pair.0.x);
        nbt.put_int("pairz", pair.0.z);
        if properties.r#type == ChestType::Right {
            nbt.put_bool("pairlead", true);
        }
    }

    Some(nbt)
}

use rustc_hash::{FxHashMap, FxHashSet};

impl PumpkinError for GetBlockError {
    fn is_kick(&self) -> bool {
        false
    }

    fn severity(&self) -> tracing::Level {
        tracing::Level::WARN
    }

    fn client_kick_reason(&self) -> Option<String> {
        None
    }
}

/// Represents a Minecraft world, containing entities, players, and the underlying level data.
///
/// Each dimension (Overworld, Nether, End) typically has its own `World`.
///
/// **Key Responsibilities:**
///
/// - Manages the `Level` instance for handling chunk-related operations.
/// - Stores and tracks active `Player` entities within the world.
/// - Provides a central hub for interacting with the world's entities and environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerRemovalReason {
    Disconnect,
    DimensionTransfer,
}

pub struct World {
    /// Represents the World's Unique Identifier
    pub uuid: Uuid,
    /// The underlying level, responsible for chunk management and terrain generation.
    pub level: Arc<Level>,
    pub level_info: Arc<ArcSwap<LevelData>>,
    /// A map of active players within the world, keyed by their unique UUID.
    pub(crate) players: PlayerMembership,
    /// A map of active entities within the world, keyed by their unique UUID.
    /// This does not include players.
    pub entities: ArcSwap<Vec<Arc<dyn EntityBase>>>,
    /// The world's scoreboard, used for tracking scores, objectives, and display information.
    pub scoreboard: std::sync::Mutex<Scoreboard>,
    /// The world's worldborder, defining the playable area and controlling its expansion or contraction.
    pub worldborder: std::sync::Mutex<Worldborder>,
    /// The world's time, including counting ticks for weather, time cycles, and statistics.
    pub level_time: std::sync::Mutex<LevelTime>,
    /// The type of dimension the world is in.
    pub dimension: Dimension,
    pub sea_level: i32,
    pub min_y: i32,
    /// The world's weather, including rain and thunder levels.
    pub weather: std::sync::Mutex<Weather>,
    /// Block Behaviour
    pub block_registry: Arc<BlockRegistry>,
    pub server: Weak<Server>,
    synced_block_event_queue: std::sync::Mutex<Vec<BlockEvent>>,
    /// A map of unsent block changes, keyed by block position.
    unsent_block_changes: std::sync::Mutex<HashMap<BlockPos, BlockStateId>>,
    /// Persisted vanilla POI storage for portal and villager lookups.
    pub portal_poi: std::sync::Mutex<portal::PortalPoiStorage>,
    /// Villager job sites and their current owners.
    pub villager_poi: std::sync::Mutex<villager_poi::VillagerPoiStorage>,
    /// Active raids in this world.
    pub raids: std::sync::Mutex<raid::Raids>,
    /// End Dragon fight manager (only present in `THE_END` dimension).
    pub dragon_fight: Option<std::sync::Mutex<dragon_fight::DragonFight>>,
    pub spawn_state: ArcSwap<SpawnState>,
    pub(crate) active_chunks: active_chunks::ActiveChunkTracker,
    /// Block entities indexed by chunk, so ticking only visits the currently
    /// active chunks instead of scanning every loaded block entity each tick.
    pub block_entities: DashMap<Vector2<i32>, FxHashMap<BlockPos, Arc<dyn BlockEntity>>>,
    pending_block_entity_migrations: crossbeam::queue::SegQueue<Vector2<i32>>,
    /// Persistent custom data for the world (matching Bukkit's `PersistentDataHolder`)
    pub custom_data: std::sync::Mutex<NbtCompound>,
    /// Persistent custom data for block entities at specific positions
    pub custom_block_entity_data: DashMap<BlockPos, NbtCompound>,
    /// Entity tracker responsible for tracking entity visibility and sending delta/status packets to watchers.
    pub entity_tracker: entity_tracker::EntityTracker,
}

#[derive(Clone, Copy)]
pub(crate) enum BlockBreakingProgress {
    Start { stage: i32, speed: f32 },
    Update { stage: i32, speed: Option<f32> },
    Stop,
}

impl PartialEq for World {
    fn eq(&self, other: &Self) -> bool {
        self.uuid == other.uuid
    }
}

impl Eq for World {}

fn traverse_vibration_blocks(
    from: Vector3<f64>,
    to: Vector3<f64>,
    mut test: impl FnMut(pumpkin_util::math::position::BlockPos) -> bool,
) -> bool {
    if from == to {
        return false;
    }

    let delta = Vector3::new(to.x - from.x, to.y - from.y, to.z - from.z);
    let start = Vector3::new(
        from.x + delta.x * 1.0e-7,
        from.y + delta.y * 1.0e-7,
        from.z + delta.z * 1.0e-7,
    );
    let end = Vector3::new(
        to.x - delta.x * 1.0e-7,
        to.y - delta.y * 1.0e-7,
        to.z - delta.z * 1.0e-7,
    );
    let mut block = pumpkin_util::math::position::BlockPos::floored(start.x, start.y, start.z);
    let end_block = pumpkin_util::math::position::BlockPos::floored(end.x, end.y, end.z);
    if test(block) {
        return true;
    }

    let steps = [
        delta.x.signum() as i32,
        delta.y.signum() as i32,
        delta.z.signum() as i32,
    ];
    let origin = [start.x, start.y, start.z];
    let direction = [delta.x, delta.y, delta.z];
    let mut t_max = [f64::INFINITY; 3];
    let mut t_delta = [f64::INFINITY; 3];
    let mut cell = [block.0.x, block.0.y, block.0.z];
    for axis in 0..3 {
        if steps[axis] != 0 {
            let boundary = f64::from(cell[axis] + i32::from(steps[axis] > 0));
            t_max[axis] = (boundary - origin[axis]) / direction[axis];
            t_delta[axis] = 1.0 / direction[axis].abs();
        }
    }

    while block != end_block {
        // Strict comparisons intentionally resolve equal crossings Z, then Y, then X.
        let axis = if t_max[0] < t_max[1] {
            if t_max[0] < t_max[2] { 0 } else { 2 }
        } else if t_max[1] < t_max[2] {
            1
        } else {
            2
        };
        cell[axis] += steps[axis];
        match axis {
            0 => block.0.x = cell[0],
            1 => block.0.y = cell[1],
            _ => block.0.z = cell[2],
        }
        t_max[axis] += t_delta[axis];
        if test(block) {
            return true;
        }
    }
    false
}

impl World {
    #[must_use]
    pub fn load(
        level: Arc<Level>,
        level_info: Arc<ArcSwap<LevelData>>,
        dimension: Dimension,
        block_registry: Arc<BlockRegistry>,
        server: Weak<Server>,
    ) -> Self {
        // TODO
        let generation_settings = NoiseSettings::from_dimension(&dimension);
        let level_data = level_info.load();
        let (day_time, game_time) = (level_data.day_time, level_data.game_time);

        // Load portal POI from disk (PoiStorage::new automatically loads from disk if files exist)
        let portal_poi = portal::PortalPoiStorage::new(level.level_folder.poi_folder.clone());
        let dragon_fight = (dimension.minecraft_name == Dimension::THE_END.minecraft_name)
            .then(|| std::sync::Mutex::new(dragon_fight::DragonFight::new()));

        let custom_data_path = level
            .level_folder
            .root_folder
            .join("pumpkin_custom_data.nbt");
        let custom_data = if custom_data_path.exists()
            && let Ok(bytes) = std::fs::read(&custom_data_path)
            && let Ok(nbt) = pumpkin_nbt::Nbt::read_unnamed(
                &mut pumpkin_nbt::deserializer::NbtReadHelperJava::new(&mut std::io::Cursor::new(
                    bytes,
                )),
            ) {
            nbt.root_tag
        } else {
            NbtCompound::new()
        };

        Self {
            uuid: Uuid::new_v4(),
            level,
            level_info,
            players: PlayerMembership::default(),
            entities: ArcSwap::new(Arc::new(Vec::new())),
            scoreboard: std::sync::Mutex::new(Scoreboard::default()),
            worldborder: std::sync::Mutex::new(Worldborder::new(
                0.0,
                0.0,
                5.999_996_8E7,
                0,
                5,
                300,
            )),
            level_time: std::sync::Mutex::new({
                let mut time = LevelTime::new();
                time.load_from(day_time, game_time);
                time
            }),
            dimension,
            weather: std::sync::Mutex::new(Weather::new()),
            block_registry,
            sea_level: generation_settings.sea_level,
            min_y: i32::from(generation_settings.shape.min_y),
            synced_block_event_queue: std::sync::Mutex::new(Vec::new()),
            unsent_block_changes: std::sync::Mutex::new(HashMap::new()),
            portal_poi: std::sync::Mutex::new(portal_poi),
            villager_poi: std::sync::Mutex::new(villager_poi::VillagerPoiStorage::default()),
            raids: std::sync::Mutex::new(raid::Raids::default()),
            dragon_fight,
            spawn_state: ArcSwap::new(Arc::new(SpawnState::empty())),
            active_chunks: active_chunks::ActiveChunkTracker::default(),
            server,
            block_entities: DashMap::new(),
            pending_block_entity_migrations: crossbeam::queue::SegQueue::new(),
            custom_data: std::sync::Mutex::new(custom_data),
            custom_block_entity_data: DashMap::new(),
            entity_tracker: entity_tracker::EntityTracker::new(),
        }
    }

    pub fn update_active_chunks(&self) {
        let sim_dist = self.server.upgrade().map_or(10, |s| {
            s.advanced_config.networking.java.simulation_distance.get()
        }) as i32;
        let spectators_generate_chunks =
            self.level_info.load().game_rules.spectators_generate_chunks;
        let players = self.players.load();
        let newly_active = self
            .active_chunks
            .update_players(players.iter().filter_map(|player| {
                if player.is_spectator() && !spectators_generate_chunks {
                    return None;
                }
                Some((
                    player.gameprofile.id,
                    ActivePlayerArea {
                        center: player.get_entity().chunk_pos.load(),
                        simulation_distance: sim_dist,
                    },
                ))
            }));

        for pos in newly_active {
            if self.level.is_chunk_loaded(&pos) && self.active_chunks.mark_loaded_active(&pos) {
                self.migrate_pending_block_entities(pos);
            }
        }

        let active_snapshot = self.active_chunks.snapshot();
        for change in self.level.loaded_chunk_changes() {
            match change {
                pumpkin_world::level::LoadedChunkChange::Loaded(pos) => {
                    if active_snapshot.contains(&pos)
                        && self.level.is_chunk_loaded(&pos)
                        && self.active_chunks.mark_loaded_active(&pos)
                    {
                        self.migrate_pending_block_entities(pos);
                    }
                }
                pumpkin_world::level::LoadedChunkChange::Unloaded(pos) => {
                    if !self.level.is_chunk_loaded(&pos) {
                        self.active_chunks.mark_unloaded(&pos);
                    }
                }
            }
        }
        let mut pending_migrations = FxHashSet::default();
        while let Some(pos) = self.pending_block_entity_migrations.pop() {
            pending_migrations.insert(pos);
        }
        for pos in pending_migrations {
            if active_snapshot.contains(&pos) && self.level.is_chunk_loaded(&pos) {
                self.migrate_pending_block_entities(pos);
            }
        }
        let spawnable_chunks = self.active_chunks.loaded_active_count() as i32;

        self.spawn_state.store(Arc::new(SpawnState::new(
            spawnable_chunks,
            &self.entities,
            self,
        )));
    }

    pub fn get_lighting_config(&self) -> LightingEngineConfig {
        self.server
            .upgrade()
            .map(|s| s.advanced_config.world.lighting)
            .unwrap_or_default()
    }

    /// Get the world folder name (e.g., `world`, `world_nether`, `world_the_end`).
    /// Falls back to "world" if the name cannot be determined.
    pub fn get_world_name(&self) -> &str {
        self.level
            .level_folder
            .root_folder
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("world")
    }

    /// Returns the configured shared world spawn block position and rotation.
    #[must_use]
    pub fn get_spawn_location(&self) -> (BlockPos, f32, f32) {
        let level_info = self.level_info.load();
        (
            BlockPos::new(level_info.spawn_x, level_info.spawn_y, level_info.spawn_z),
            level_info.spawn_yaw,
            level_info.spawn_pitch,
        )
    }

    #[must_use]
    pub fn is_in_spawn_protection(&self, player: &Player, position: &BlockPos) -> bool {
        if player.permission_lvl.load() == pumpkin_util::permission::PermissionLvl::Four {
            return false;
        }

        let Some(server) = self.server.upgrade() else {
            return false;
        };

        let radius = server.basic_config.spawn_protection;
        if radius == 0 {
            return false;
        }

        let radius = i32::try_from(radius).unwrap_or(i32::MAX);
        let spawn = self.get_spawn_location().0;
        let dx = (spawn.0.x - position.0.x).abs();
        let dz = (spawn.0.z - position.0.z).abs();

        dx <= radius && dz <= radius
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        let entities = self.entities.load_full();
        for entity in entities.iter() {
            self.save_entity(entity).await?;
        }
        drop(entities);

        // Stop retaining live entity/client/world ownership cycles only after
        // every entity save succeeded. On failure the live index is deliberately
        // retained and the error is propagated instead of being treated as a
        // successful save.
        self.entities.store(Arc::new(Vec::new()));
        self.entity_tracker.clear();
        self.spawn_state.store(Arc::new(SpawnState::empty()));

        let chunks: Vec<Vector2<i32>> = self
            .block_entities
            .iter()
            .map(|chunk_block_entities| *chunk_block_entities.key())
            .collect();
        for chunk_pos in chunks {
            self.save_block_entities(chunk_pos);
        }

        // Save portal POI to disk
        let save_result = self
            .portal_poi
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .save_all();
        save_result.map_err(|error| format!("failed to save portal POI: {error}"))?;

        self.level.shutdown().await
    }

    /// Serializes a live entity into its current chunk's entity data. The live
    /// entity list is the source of truth while a chunk is loaded (its saved NBT
    /// is consumed on load), so this loads/merges the live snapshot into the
    /// serialized destination chunk. Repeated saves replace the same UUID rather
    /// than accumulating duplicate entity records.
    async fn save_entity(&self, entity: &Arc<dyn EntityBase>) -> Result<(), String> {
        let base_entity = entity.get_entity();
        if base_entity.is_removed() {
            return Ok(());
        }
        let mut nbt = NbtCompound::new();
        entity.write_nbt(&mut nbt);
        // Choose the destination from the serialized position, not from an
        // earlier block_pos read. This prevents an eviction boundary move from
        // writing a new-position snapshot into the old chunk.
        let current_chunk = nbt
            .get_list("Pos")
            .and_then(|position| {
                let [x, _, z] = position else {
                    return None;
                };
                Some(Vector2::new(
                    (x.extract_double()?.floor() as i32) >> 4,
                    (z.extract_double()?.floor() as i32) >> 4,
                ))
            })
            .unwrap_or_else(|| base_entity.block_pos.load().chunk_position());
        // The old shutdown path overflowed the main task while re-entering the
        // async entity loader. Keep the bounded direct IO future heap-backed and
        // run it as a separate Tokio task; this does not enlarge the OS stack or
        // introduce a generation fallback.
        let level = self.level.clone();
        let entity_uuid = base_entity.entity_uuid;
        let previous_chunk = base_entity.last_pos.load();
        let previous_chunk = Vector2::new(
            (previous_chunk.x.floor() as i32) >> 4,
            (previous_chunk.z.floor() as i32) >> 4,
        );
        let stale_chunk = (previous_chunk != current_chunk).then_some(previous_chunk);
        let save_result = tokio::spawn(async move {
            Box::pin(level.save_entity_nbt_with_stale_chunk(
                current_chunk,
                entity_uuid,
                nbt,
                stale_chunk,
            ))
            .await
        })
        .await
        .map_err(|error| format!("entity save task failed: {error}"))?;
        save_result.map_err(|error| {
            format!(
                "failed to save entity {} in chunk {:?}: {error}",
                base_entity.entity_id, current_chunk
            )
        })
    }

    /// Broadcasts an entity status update / event to all players tracking the specified entity,
    /// and to the entity itself if it is a player.
    /// Matching Vanilla's `ServerLevel.broadcastEntityEvent(entity, event)`.

    /// Broadcasts a damage event to all players tracking the specified entity,
    /// and to the entity itself if it is a player.
    /// Matching Vanilla's `ServerLevel.broadcastDamageEvent(entity, source)`.

    /// Sends an entity status update to all players tracking the specified entity.

    #[must_use]

    pub fn set_difficulty(&self, difficulty: Difficulty) {
        let current_info = self.level_info.load();
        let mut new_info = (**current_info).clone();
        new_info.difficulty = difficulty;
        self.level_info.store(Arc::new(new_info));
    }

    pub fn get_game_rule(&self, rule: &GameRule) -> GameRuleValue<i64, bool> {
        let level_info = self.level_info.load();
        match level_info.game_rules.get(rule) {
            GameRuleValue::Int(v) => GameRuleValue::Int(*v),
            GameRuleValue::Bool(v) => GameRuleValue::Bool(*v),
        }
    }

    pub fn set_game_rule(&self, rule: &GameRule, value: GameRuleValue<i64, bool>) {
        let current_info = self.level_info.load();
        let mut new_info = (**current_info).clone();
        match (new_info.game_rules.get_mut(rule), value) {
            (GameRuleValue::Int(target), GameRuleValue::Int(val)) => {
                *target = val;
            }
            (GameRuleValue::Bool(target), GameRuleValue::Bool(val)) => {
                *target = val;
            }
            _ => {}
        }
        self.level_info.store(Arc::new(new_info));
    }

    pub fn add_synced_block_event(&self, pos: BlockPos, r#type: u8, data: u8) {
        let mut queue = self
            .synced_block_event_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.push(BlockEvent { pos, r#type, data });
    }

    pub fn flush_synced_block_events(self: &Arc<Self>) {
        // THIS IS IMPORTANT
        // it prevents deadlocks and also removes the need to wait for a lock when adding a new synced block
        let events = {
            let mut queue = self
                .synced_block_event_queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *queue)
        };

        for event in events {
            let block = self.get_block(&event.pos);
            if !self.block_registry.on_synced_block_event(
                block,
                self,
                &event.pos,
                event.r#type,
                event.data,
            ) {
                continue;
            }
            let chunk_pos = event.pos.chunk_position();
            self.broadcast_to_chunk_editioned(
                chunk_pos,
                &CBlockEvent::new(
                    event.pos,
                    event.r#type,
                    event.data,
                    VarInt(block.id.as_u16() as i32),
                ),
                &CBedrockBlockEvent {
                    block_position: event.pos,
                    event_type: event.r#type.into(),
                    event_value: event.data.into(),
                },
            );
        }
    }

    /// Broadcasts a packet to all connected players within the world.
    /// Please avoid this as we want to replace it with `broadcast_editioned`
    ///
    /// Sends the specified packet to every player currently logged in to the world.
    ///
    /// **Note:** This function acquires a lock on the `current_players` map, ensuring thread safety.

    // This should replace broadcast_packet_all at some point

    /// Broadcasts the skin layers of a player, encoding the metadata for each Java client's own
    /// protocol version since the tracked data index differs between versions.

    /// Broadcasts a packet to all connected players within the world, excluding the specified players.
    ///
    /// Sends the specified packet to every player currently logged in to the world, excluding the players listed in the `except` parameter.
    ///
    /// **Note:** This function acquires a lock on the `current_players` map, ensuring thread safety.

    /// Plays a custom sound event by identifier for all players in range.

    /// Spawns a cluster of particles in the world for all players in range.

    /// Plays a Bedrock level sound for players close enough to hear it.

    pub fn register_block_change(&self, position: BlockPos, block_state_id: BlockStateId) {
        self.unsent_block_changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(position, block_state_id);
    }

    /// Queues block state changes for broadcast to nearby players.
    ///
    /// Call [`flush_block_updates`](Self::flush_block_updates) afterward to send the packets.
    pub fn queue_block_updates(&self, changes: &[(BlockPos, BlockStateId)]) {
        let mut guard = self
            .unsent_block_changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (pos, state_id) in changes {
            guard.insert(*pos, *state_id);
        }
    }

    #[expect(clippy::too_many_lines)]
    pub fn flush_block_updates(&self) {
        let mut block_state_updates_by_chunk_section: HashMap<
            Vector3<i32>,
            Vec<(BlockPos, BlockStateId)>,
        > = HashMap::new();
        let changes = {
            let mut guard = self
                .unsent_block_changes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *guard)
        };
        for (position, block_state_id) in changes {
            let chunk_section = chunk_section_from_pos(&position);
            block_state_updates_by_chunk_section
                .entry(chunk_section)
                .or_default()
                .push((position, block_state_id));
        }

        // TODO: only send packet to players who have the chunks loaded
        // TODO: Send light updates to update the wire directly next to a broken block
        for (chunk_section, updates) in block_state_updates_by_chunk_section {
            if updates.is_empty() {
                continue;
            }
            let chunk_pos = Vector2::new(chunk_section.x, chunk_section.z);
            if updates.len() == 1 {
                let (block_pos, block_state_id) = updates[0];
                let be_block_id = BlockState::to_be_network_id(block_state_id);
                self.broadcast_to_chunk_editioned(
                    chunk_pos,
                    &CBlockUpdate::new(block_pos, i32::from(block_state_id.as_u16()).into()),
                    &pumpkin_protocol::bedrock::client::CUpdateBlock::new(block_pos, be_block_id),
                );
                if let Some(block_entity) = self.get_block_entity(&block_pos)
                    && let Some(nbt) = block_entity.chunk_data_nbt()
                {
                    let bytes = pumpkin_nbt::Nbt::from(nbt).write_unnamed();
                    self.broadcast_to_chunk(
                        chunk_pos,
                        &CBlockEntityData::new(
                            block_pos,
                            VarInt(block_entity.get_id() as i32),
                            bytes.as_ref().into(),
                        ),
                    );
                }
                if let Some(data) = self.bedrock_block_entity_data(block_state_id, block_pos) {
                    self.broadcast_to_chunk_bedrock(
                        chunk_pos,
                        &CBlockActorData::new(block_pos, data),
                    );
                }
            } else {
                let players = self.players.load();
                let mut java_recipients = Vec::new();

                let recipients = players.iter().filter(|p| {
                    p.watched_section
                        .load()
                        .is_within_distance(chunk_pos.x, chunk_pos.y)
                });

                let mut bedrock_packets = Vec::new();
                for (block_pos, block_state_id) in &updates {
                    let be_block_id = BlockState::to_be_network_id(*block_state_id);
                    let update_packet = pumpkin_protocol::bedrock::client::CUpdateBlock::new(
                        *block_pos,
                        be_block_id,
                    );
                    let actor_packet = self
                        .bedrock_block_entity_data(*block_state_id, *block_pos)
                        .map(|data| CBlockActorData::new(*block_pos, data));
                    bedrock_packets.push((update_packet, actor_packet));
                }

                let mut bedrock_recipients = Vec::new();
                for p in recipients {
                    match p.client.as_ref() {
                        ClientPlatform::Java(_) => java_recipients.push(p),
                        ClientPlatform::Bedrock(be_client) => {
                            bedrock_recipients.push(be_client);
                        }
                    }
                }

                for be_client in bedrock_recipients {
                    for (update_packet, actor_packet) in &bedrock_packets {
                        if let Ok(data) = be_client.serialize_packet(update_packet) {
                            be_client.try_enqueue_packet(data);
                        }
                        if let Some(actor_packet) = actor_packet
                            && let Ok(data) = be_client.serialize_packet(actor_packet)
                        {
                            be_client.try_enqueue_packet(data);
                        }
                    }
                }

                let recipients_by_version =
                    Self::collect_java_recipients_by_version(java_recipients.into_iter());
                Self::broadcast_java_grouped(
                    &CMultiBlockUpdate::new(&updates),
                    recipients_by_version,
                );

                for (block_pos, _) in &updates {
                    if let Some(block_entity) = self.get_block_entity(block_pos)
                        && let Some(nbt) = block_entity.chunk_data_nbt()
                    {
                        let bytes = pumpkin_nbt::Nbt::from(nbt).write_unnamed();
                        self.broadcast_to_chunk(
                            chunk_pos,
                            &CBlockEntityData::new(
                                *block_pos,
                                VarInt(block_entity.get_id() as i32),
                                bytes.as_ref().into(),
                            ),
                        );
                    }
                }
            }

            let mut bedrock_water_packets = Vec::new();
            for (block_pos, block_state_id) in &updates {
                let water_state = bedrock_water_state(*block_state_id);
                let packet = pumpkin_protocol::bedrock::client::CUpdateBlock::with_layer(
                    *block_pos,
                    BlockState::to_be_network_id(water_state),
                    1,
                );
                bedrock_water_packets.push(packet);
            }

            if !bedrock_water_packets.is_empty() {
                let players = self.players.load();
                let recipients = players.iter().filter(|player| {
                    player
                        .watched_section
                        .load()
                        .is_within_distance(chunk_pos.x, chunk_pos.y)
                });
                for player in recipients {
                    if let ClientPlatform::Bedrock(client) = player.client.as_ref() {
                        for packet in &bedrock_water_packets {
                            if let Ok(data) = client.serialize_packet(packet) {
                                client.try_enqueue_packet(data);
                            }
                        }
                    }
                }
            }
        }
    }

    // FlowingFluid.getFlow()

    // FlowingFluid.isSolidFace()

    // For adjusting movement

    /// Vanilla's `BlockView.getDismountHeight()`.
    /// Returns the Y surface height for dismounting at the given block position,
    /// or `f64::NEG_INFINITY` if no valid surface exists.

    pub fn get_world_age(&self) -> i64 {
        self.level_time
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .world_age
    }

    pub fn get_time_of_day(&self) -> i64 {
        self.level_time
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .time_of_day
    }

    pub fn set_time_of_day(&self, time: i64) {
        let level_time = {
            let mut guard = self
                .level_time
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.set_time(time);
            guard.clone()
        };
        level_time.send_time(self);
    }

    pub fn is_raining(&self) -> bool {
        self.weather
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .raining
    }

    pub fn is_raining_at(&self, pos: &BlockPos) -> bool {
        if !self.is_raining() {
            return false;
        }
        if self.get_heightmap_height(MotionBlocking, pos.0.x, pos.0.z) + 1 > pos.0.y {
            return false;
        }
        self.can_see_sky(pos)
            && self
                .get_biome(pos)
                .weather
                .is_rain_at(pos.0.x, pos.0.y, pos.0.z, self.sea_level)
    }

    pub fn set_raining(&self, raining: bool) {
        if let Some(server) = self.server.upgrade() {
            let world_arc = server.get_world_from_dimension(&self.dimension);
            let mut event =
                crate::plugin::api::events::world::weather_change::WeatherChangeEvent::new(
                    world_arc, raining,
                );
            server.plugin_manager.fire_blocking(&server, &mut event);
            if event.cancelled {
                return;
            }
        }
        let mut weather = self
            .weather
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if weather.raining != raining {
            let thunder = weather.thundering;
            weather.set_weather_parameters(self, 0, 0, raining, thunder);
        }
    }

    pub fn is_thundering(&self) -> bool {
        self.weather
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .thundering
    }

    pub fn set_thundering(&self, thundering: bool) {
        if let Some(server) = self.server.upgrade() {
            let world_arc = server.get_world_from_dimension(&self.dimension);
            let mut event =
                crate::plugin::api::events::world::weather_change::ThunderChangeEvent::new(
                    world_arc, thundering,
                );
            server.plugin_manager.fire_blocking(&server, &mut event);
            if event.cancelled {
                return;
            }
        }
        let mut weather = self
            .weather
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if weather.thundering != thundering {
            let raining = weather.raining;
            weather.set_weather_parameters(self, 0, 0, raining, thundering);
        }
    }

    /// Gets the y position of the first non air block from the top down

    #[allow(clippy::too_many_lines)]
    #[expect(clippy::too_many_lines)]

    pub fn explode(
        self: &Arc<Self>,
        position: Vector3<f64>,
        power: f32,
        interaction: ExplosionInteraction,
    ) {
        self.explode_with_calculator(position, power, interaction, None);
    }

    pub fn explode_with_calculator(
        self: &Arc<Self>,
        position: Vector3<f64>,
        power: f32,
        interaction: ExplosionInteraction,
        damage_calculator: Option<Arc<dyn ExplosionDamageCalculator>>,
    ) {
        let block_interaction = self.get_block_interaction(interaction);
        let mut explosion = Explosion::new(power, position, block_interaction);
        if let Some(calc) = damage_calculator {
            explosion = explosion.with_damage_calculator(calc);
        }
        self.run_explosion(&explosion, position, power);
    }

    pub fn explode_tnt_minecart(self: &Arc<Self>, position: Vector3<f64>, power: f32) {
        let block_interaction = self.get_block_interaction(ExplosionInteraction::Tnt);
        let explosion = Explosion::new(power, position, block_interaction).preserving_rails();
        self.run_explosion(&explosion, position, power);
    }

    #[must_use]
    pub fn get_block_interaction(&self, interaction: ExplosionInteraction) -> BlockInteraction {
        let game_rules = &self.level_info.load().game_rules;
        match interaction {
            ExplosionInteraction::None => BlockInteraction::Keep,
            ExplosionInteraction::Block => {
                Self::get_destroy_type(game_rules.block_explosion_drop_decay)
            }
            ExplosionInteraction::Mob => {
                if game_rules.mob_griefing {
                    Self::get_destroy_type(game_rules.mob_explosion_drop_decay)
                } else {
                    BlockInteraction::Keep
                }
            }
            ExplosionInteraction::Tnt => {
                Self::get_destroy_type(game_rules.tnt_explosion_drop_decay)
            }
            ExplosionInteraction::Trigger => BlockInteraction::TriggerBlock,
        }
    }

    #[must_use]
    pub const fn get_destroy_type(drop_decay: bool) -> BlockInteraction {
        if drop_decay {
            BlockInteraction::DestroyWithDecay
        } else {
            BlockInteraction::Destroy
        }
    }

    fn run_explosion(self: &Arc<Self>, explosion: &Explosion, position: Vector3<f64>, power: f32) {
        let mut event = crate::plugin::api::events::entity::entity_explode::EntityExplodeEvent::new(
            0, position, power,
        );
        if let Some(server) = self.server.upgrade() {
            server.plugin_manager.fire_blocking(&server, &mut event);
        }
        if event.cancelled {
            return;
        }

        let block_count = explosion.explode(self);
        let particle = if power < 2.0 {
            Particle::Explosion
        } else {
            Particle::ExplosionEmitter
        };
        for player in self.players.load().iter() {
            let mut sound_id = Sound::EntityGenericExplode as u16;
            if let ClientPlatform::Java(java_client) = player.client.as_ref() {
                sound_id = remap_sound_id_for_version(sound_id, java_client.version.load());
            }
            let sound = IdOr::<SoundEvent>::Id(sound_id);
            if player.position().squared_distance_to_vec(&position) > 4096.0 {
                continue;
            }
            player.try_send_client_packet(&CExplosion::new(
                position,
                power,
                block_count as i32,
                None,
                VarInt(particle as i32),
                sound,
            ));
        }
    }

    #[allow(clippy::too_many_lines)]

    /// Returns true if enough players are sleeping and we should skip the night.
    pub fn should_skip_night(&self) -> bool {
        let players = self.players.load();

        let player_count = players.len();
        let sleeping_player_count = players
            .iter()
            .filter(|player| {
                player
                    .sleeping_since
                    .load()
                    .is_some_and(|since| since >= 100)
            })
            .count();
        drop(players);

        if player_count == 0 {
            return false;
        }

        let sleep_percentage = self
            .level_info
            .load()
            .game_rules
            .players_sleeping_percentage
            .clamp(0, 100);
        let required_sleeping =
            ((player_count as f64 * sleep_percentage as f64) / 100.0).ceil() as usize;
        let required_sleeping = required_sleeping.max(1);

        sleeping_player_count >= required_sleeping
    }

    // NOTE: This function doesn't actually await on anything, it just spawns two tokio tasks
    /// IMPORTANT: Chunks have to be non-empty
    fn spawn_world_entity_chunks(self: &Arc<Self>, player: Arc<Player>, chunks: Vec<Vector2<i32>>) {
        #[cfg(debug_assertions)]
        let inst = std::time::Instant::now();

        // Note: `chunks` originates from `Cylindrical::changed_chunks`, which is
        // already ordered from closest to farthest from center by the precompiled
        // cylindrical chunk view LUT. No re-sorting needed.

        let mut entity_receiver = self.level.receive_entity_chunks(chunks);
        let level = self.level.clone();
        let world = self.clone();

        player.clone().spawn_task(async move {
            'main: loop {
                let recv_result = tokio::select! {
                    () = player.client.await_close_interrupt() => {
                        debug!("Canceling player packet processing");
                        None
                    },
                    recv_result = entity_receiver.recv() => {
                        recv_result
                    }
                };

                let Some((chunk_weak, first_load)) = recv_result else {
                    break;
                };

                let Some(chunk) = chunk_weak.upgrade() else {
                    continue;
                };

                let position = Vector2::new(chunk.x, chunk.z);

                if !level.is_chunk_watched(&position) {
                    // No longer watched: don't make its entities live. Leave the
                    // serialized data untouched so the normal unload path persists
                    // it as-is (nothing went live, so there is nothing to save).
                    trace!(
                        "Received entity chunk {:?}, but it is no longer watched; leaving it for the unload path",
                        &position
                    );
                    continue 'main;
                }

                if first_load {
                    // First watcher: consume the serialized entities and make them
                    // live. The live entity list becomes the single source of
                    // truth, so the chunk's NBT is taken (cleared) to avoid keeping
                    // a duplicate copy that would be re-appended on the next unload
                    // and doubled on every reload.
                    let entity_nbts = std::mem::take(
                        &mut *chunk
                            .data
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                    );
                    for entity_nbt in &entity_nbts {
                        let Some(id) = entity_nbt.get_string("id") else {
                            debug!("Entity has no ID");
                            continue;
                        };
                        let Some(entity_type) =
                            EntityType::from_name(id.strip_prefix("minecraft:").unwrap_or(id))
                        else {
                            warn!("Entity has no valid Entity Type {id}");
                            continue;
                        };

                        // Keep the persisted UUID so the entity keeps its identity
                        // across reloads (matching vanilla); only fall back to a
                        // fresh one if it is missing/corrupt.
                        let uuid = entity_nbt.get_uuid("UUID").unwrap_or_else(Uuid::new_v4);
                        // Pos is zero since it will be read from nbt.
                        let entity =
                            from_type(entity_type, Vector3::new(0.0, 0.0, 0.0), &world, uuid);
                        entity.read_nbt_non_mut(entity_nbt);
                        entity.init_data_tracker();

                        let base_entity = entity.get_entity();
                        // Clear velocity so the client does not replay the drop
                        // animation; residual velocity from the original drop is
                        // stale data.
                        base_entity.velocity.store(Vector3::default());

                        // Tracker owns pairing: spawn packets for every watcher.
                        world.add_entity_silent(entity);
                    }
                }
                // Already-live chunk: tracker pairs on its next pass.
            }

            #[cfg(debug_assertions)]
            debug!("Chunks queued after {}ms", inst.elapsed().as_millis());
        });
    }

    /// Gets a `Player` by an entity id
    pub fn get_player_by_id(&self, id: i32) -> Option<Arc<Player>> {
        for player in self.players.load().iter() {
            if player.entity_id() == id {
                return Some(player.clone());
            }
        }
        None
    }

    /// Gets an entity by an entity id
    pub fn get_entity_by_id(&self, id: i32) -> Option<Arc<dyn EntityBase>> {
        for entity in self.entities.load().iter() {
            if entity.get_entity().entity_id == id {
                return Some(entity.clone());
            }
            if let Some(dragon) = entity
                .cast_any()
                .downcast_ref::<crate::entity::boss::ender_dragon::EnderDragonEntity>(
            ) && let Some(part) = dragon.parts.iter().find(|part| part.entity.entity_id == id)
            {
                return Some(part.clone() as Arc<dyn EntityBase>);
            }
        }
        for player in self.players.load().iter() {
            if player.get_entity().entity_id == id {
                return Some(player.clone() as Arc<dyn EntityBase>);
            }
        }
        None
    }

    /// Gets a `Player` by a username
    pub fn get_player_by_name(&self, name: &str) -> Option<Arc<Player>> {
        for player in self.players.load().iter() {
            if player.gameprofile.name.eq_ignore_ascii_case(name) {
                return Some(player.clone());
            }
        }
        None
    }

    // Gets all entities at a Box
    pub fn get_all_at_box(&self, aabb: &BoundingBox) -> Vec<Arc<dyn EntityBase>> {
        let entities_guard = self.entities.load();
        let players_guard = self.players.load();

        entities_guard
            .iter()
            .map(|e| e.clone() as Arc<dyn EntityBase>)
            .chain(
                players_guard
                    .iter()
                    .map(|p| p.clone() as Arc<dyn EntityBase>),
            )
            .filter(|entity| entity.get_entity().bounding_box.load().intersects(aabb))
            .collect()
    }

    // Gets all non Player entities at a Box
    pub fn get_entities_at_box(&self, aabb: &BoundingBox) -> Vec<Arc<dyn EntityBase>> {
        self.entities
            .load()
            .iter()
            .filter(|entity| entity.get_entity().bounding_box.load().intersects(aabb))
            .cloned()
            .collect()
    }

    // Gets all Player entities at a Box
    pub fn get_players_at_box(&self, aabb: &BoundingBox) -> Vec<Arc<Player>> {
        let players_guard = self.players.load();
        players_guard
            .iter()
            .filter(|player| player.get_entity().bounding_box.load().intersects(aabb))
            .cloned()
            .collect()
    }

    /// Retrieves a player by their unique UUID.
    ///
    /// This function searches the world's active player list for a player with the specified UUID.
    /// If found, it returns an `Arc<Player>` reference to the player. Otherwise, it returns `None`.
    ///
    /// # Arguments
    ///
    /// * `id`: The UUID of the player to retrieve.
    ///
    /// # Returns
    ///
    /// An `Option<Arc<Player>>` containing the player if found, or `None` if not.
    pub fn get_player_by_uuid(&self, id: uuid::Uuid) -> Option<Arc<Player>> {
        self.players
            .load()
            .iter()
            .find(|p| p.gameprofile.id == id)
            .cloned()
    }

    /// Retrieves an entity by their unique UUID.
    ///
    /// This function searches the world's entities for one with the specified UUID.
    /// If found, it returns an `Arc<dyn EntityBase>` reference to that entity. Otherwise, it returns `None`.
    ///
    /// # Arguments
    ///
    /// * `id`: The UUID of the entity to retrieve.
    ///
    /// # Returns
    ///
    /// An `Option<Arc<dyn EntityBase>>` containing the player if found, or `None` if not.
    pub fn get_entity_by_uuid(&self, id: uuid::Uuid) -> Option<Arc<dyn EntityBase>> {
        self.entities
            .load()
            .iter()
            .find(|p| p.get_entity().entity_uuid == id)
            .cloned()
    }

    /// Gets a list of players whose location equals the given position in the world.
    ///
    /// It iterates through the players in the world and checks their location. If the player's location matches the
    /// given position, it will add this to a `Vec` which it later returns. If no
    /// player was found in that position, it will just return an empty `Vec`.
    ///
    /// # Arguments
    ///
    /// * `position`: The position the function will check.
    pub fn get_players_by_pos(&self, position: BlockPos) -> Vec<Arc<Player>> {
        self.players
            .load()
            .iter()
            .filter_map(|player| {
                let player_block_pos = player.get_entity().block_pos.load().0;
                (position.0.x == player_block_pos.x
                    && position.0.y == player_block_pos.y
                    && position.0.z == player_block_pos.z)
                    .then(|| Arc::clone(player))
            })
            .collect::<_>()
    }

    /// Gets the nearby players around a given world position.
    /// It "creates" a sphere and checks if whether players are inside
    /// and returns a `HashMap` where the UUID is the key and the `Player`
    /// object is the value.
    ///
    /// # Arguments
    /// * `pos`: The center of the sphere.
    /// * `radius`: The radius of the sphere. The higher the radius, the more area will be checked (in every direction).
    pub fn get_nearby_players(&self, pos: Vector3<f64>, radius: f64) -> Vec<Arc<Player>> {
        let radius_squared = radius.powi(2);

        self.players
            .load()
            .iter()
            .filter_map(|player| {
                let player_pos = player.get_entity().pos.load();
                (player_pos.squared_distance_to_vec(&pos) <= radius_squared).then(|| player.clone())
            })
            .collect()
    }

    pub fn get_nearby_entities(
        &self,
        pos: Vector3<f64>,
        radius: f64,
    ) -> HashMap<uuid::Uuid, Arc<dyn EntityBase>> {
        let radius_squared = radius.powi(2);

        self.entities
            .load()
            .iter()
            .filter_map(|entity| {
                let entity_pos = entity.get_entity().pos.load();
                (entity_pos.squared_distance_to_vec(&pos) <= radius_squared)
                    .then(|| (entity.get_entity().entity_uuid, entity.clone()))
            })
            .collect()
    }

    /// Closest player that satisfies `predicate`. Unlike [`Self::get_closest_player`], a nearer
    /// player failing the predicate does not hide a farther one that passes it.
    pub fn get_nearest_player(
        &self,
        pos: Vector3<f64>,
        radius: f64,
        predicate: impl Fn(&Arc<Player>) -> bool,
    ) -> Option<Arc<Player>> {
        self.get_nearby_players(pos, radius)
            .into_iter()
            .filter(|player| predicate(player))
            .min_by(|a, b| {
                a.get_entity()
                    .pos
                    .load()
                    .squared_distance_to_vec(&pos)
                    .total_cmp(&b.get_entity().pos.load().squared_distance_to_vec(&pos))
            })
    }

    /// Closest entity that satisfies `predicate`. See [`Self::get_nearest_player`] for why this is
    /// not [`Self::get_closest_entity`] followed by a check.
    pub fn get_nearest_entity(
        &self,
        pos: Vector3<f64>,
        radius: f64,
        entity_types: Option<&[&'static EntityType]>,
        predicate: impl Fn(&Arc<dyn EntityBase>) -> bool,
    ) -> Option<Arc<dyn EntityBase>> {
        self.get_nearby_entities(pos, radius)
            .into_values()
            .filter(|entity| {
                entity_types.is_none_or(|types| types.contains(&entity.get_entity().entity_type))
                    && predicate(entity)
            })
            .min_by(|a, b| {
                a.get_entity()
                    .pos
                    .load()
                    .squared_distance_to_vec(&pos)
                    .total_cmp(&b.get_entity().pos.load().squared_distance_to_vec(&pos))
            })
    }

    pub fn get_closest_player(&self, pos: Vector3<f64>, radius: f64) -> Option<Arc<Player>> {
        self.get_nearest_player(pos, radius, |_| true)
    }

    /// Gets the closest entity to a position, with optional filtering by entity type.
    ///
    /// # Arguments
    ///
    /// * `pos` - The position to search around.
    /// * `radius` - The radius to search within.
    /// * `entity_types` - Optional array of entity types to filter by. If None, all entity types are included.
    ///
    /// # Returns
    ///
    /// The closest entity that matches the filter criteria, or None if no entities are found.
    pub fn get_closest_entity(
        &self,
        pos: Vector3<f64>,
        radius: f64,
        entity_types: Option<&[&'static EntityType]>,
    ) -> Option<Arc<dyn EntityBase>> {
        self.get_nearest_entity(pos, radius, entity_types, |_| true)
    }

    /// Adds entities to the provided [`Vec`] that satisfy a particular condition and are
    /// present in the provided [`BoundingBox`].
    ///
    /// # Arguments
    ///
    /// * `list`: The `Vec` to add to.
    /// * `max_list_capacity`: The maximum capacity of `list` for adding entities. If this limit is reached, no more
    ///   entities will be added to the list. If `list` already reaches this limit, nothing happens.
    /// * `bounding_box`: The bounding box to filter any added entities.
    /// * `predicate`: A predicate function, which has to be `true` for an entity to be added to the list.
    pub fn extend_entities_in_box_where(
        &self,
        list: &mut Vec<Arc<dyn EntityBase>>,
        max_list_capacity: usize,
        bounding_box: BoundingBox,
        predicate: impl Fn(&dyn EntityBase) -> bool,
    ) {
        self.extend_entities_where(list, max_list_capacity, |e| {
            bounding_box.intersects(&e.get_entity().bounding_box.load()) && predicate(e)
        });
    }

    /// Adds entities to the provided [`Vec`] that satisfy a particular condition.
    ///
    /// # Arguments
    ///
    /// * `list`: The `Vec` to add to.
    /// * `max_list_capacity`: The maximum capacity of `list` for adding entities. If this limit is reached, no more
    ///   entities will be added to the list. If `list` already reaches this limit, nothing happens.
    /// * `predicate`: A predicate function, which has to be `true` for an entity to be added to the list.
    pub fn extend_entities_where(
        &self,
        list: &mut Vec<Arc<dyn EntityBase>>,
        max_list_capacity: usize,
        predicate: impl Fn(&dyn EntityBase) -> bool,
    ) {
        if list.len() >= max_list_capacity {
            return;
        }
        // Loop the players.
        for player in self.players.load().iter() {
            if !predicate(player.as_ref()) {
                continue;
            }
            // We add the player to the list.
            list.push(player.clone());
            // Check if the list is too big.
            if list.len() > max_list_capacity {
                return;
            }
        }
        // Same with entities.
        for entity in self.entities.load().iter() {
            if !predicate(entity.as_ref()) {
                continue;
            }
            list.push(entity.clone());
            if list.len() > max_list_capacity {
                return;
            }
            // TODO: Implement ender dragon handling
        }
    }

    /// Adds a player to the world and broadcasts a join message if enabled.
    ///
    /// This function takes a player's UUID and an `Arc<Player>` reference.
    /// It inserts the player into the world's `current_players` map using the UUID as the key.
    /// Additionally, it broadcasts a join message to all connected players in the world.
    ///
    /// # Arguments
    ///
    /// * `player`: An `Arc<Player>` reference to the player object.

    /// Must only be called after the player's own `CLogin` packet has been sent.

    /// Removes a player from the world and broadcasts a disconnect message if enabled.
    ///
    /// This function removes a player from the world based on their `Player` reference.
    /// It performs the following actions:
    ///
    /// 1. Removes the player from the `current_players` map using their UUID.
    /// 2. Broadcasts a `CRemovePlayerInfo` packet to all connected players to inform them about the player leaving.
    /// 3. Removes the player's entity from the world using its entity ID.
    /// 4. Optionally sends a disconnect message to all other players notifying them about the player leaving.
    ///
    /// # Arguments
    ///
    /// * `player`: A reference to the `Player` object to be removed.
    /// * `reason`: Whether this is a disconnect or a cross-dimension transfer.
    ///
    /// # Notes
    ///
    /// - This function assumes `broadcast_packet_expect` and `remove_entity` are defined elsewhere.
    /// - The disconnect message sending is currently optional. Consider making it a configurable option.

    #[expect(clippy::needless_pass_by_value)]
    pub fn spawn_entity_non_save(&self, entity: Arc<dyn EntityBase>) {
        let _base_entity = entity.get_entity();
        self.entity_tracker.add_entity(&entity, self);
        self.spawn_state.load().add_entity(self, entity.as_ref());

        self.entities.rcu(|current_entities| {
            let mut new_entities = (**current_entities).clone();
            new_entities.push(entity.clone());
            new_entities
        });
    }

    pub fn spawn_entity(self: &Arc<Self>, entity: Arc<dyn EntityBase>) {
        self.try_spawn_entity(entity);
    }

    pub fn try_spawn_entity(self: &Arc<Self>, entity: Arc<dyn EntityBase>) -> bool {
        let mut event = crate::plugin::api::events::entity::entity_spawn::EntitySpawnEvent::new(
            entity.get_entity().entity_id,
            entity.get_entity().entity_type.id.to_string(),
            entity.get_entity().pos.load(),
            self.clone(),
        );
        if let Some(server) = self.server.upgrade() {
            server.plugin_manager.fire_blocking(&server, &mut event);
        }
        if event.cancelled {
            return false;
        }

        entity.init_data_tracker();
        self.insert_entity_silent(entity)
    }

    #[expect(clippy::needless_pass_by_value)]
    pub fn add_entity_silent(&self, entity: Arc<dyn EntityBase>) {
        self.insert_entity_silent(entity);
    }

    #[expect(clippy::needless_pass_by_value)]
    fn insert_entity_silent(&self, entity: Arc<dyn EntityBase>) -> bool {
        let base_entity = entity.get_entity();

        // Guard against duplicate entities with the same UUID.
        // This can happen when chunk entity data is loaded while the entity
        // already exists in the world (e.g. another player is still tracking it).
        let already_exists = self
            .entities
            .load()
            .iter()
            .any(|e| e.get_entity().entity_uuid == base_entity.entity_uuid);
        if already_exists {
            return false;
        }

        // The entity stays live-only: it is written to its chunk's saved data on
        // unload (see `save_entity`), never at spawn, so it can't be both live and
        // serialized at once (which would double it on the next reload).
        self.spawn_state.load().add_entity(self, entity.as_ref());
        self.entity_tracker.add_entity(&entity, self);

        self.entities.rcu(|current_entities| {
            let mut new_entities = (**current_entities).clone();
            new_entities.push(entity.clone());
            new_entities
        });
        true
    }

    pub fn remove_entity(&self, entity: &dyn EntityBase) {
        let base_entity = entity.get_entity();
        if base_entity
            .removal_reason
            .swap(Some(RemovalReason::Discarded))
            .is_some()
        {
            return;
        }
        base_entity.removed.store(true, Ordering::Release);

        self.spawn_state.load().remove_entity(self, entity);
        self.entity_tracker.remove_entity(entity, self);
        self.entities.rcu(|current_entities| {
            let mut new_entities = (**current_entities).clone();
            new_entities.retain(|e| e.get_entity().entity_uuid != base_entity.entity_uuid);
            new_entities
        });
    }

    pub async fn remove_entities_in_chunks(
        &self,
        chunks: impl IntoIterator<Item = impl std::borrow::Borrow<Vector2<i32>>>,
    ) -> Result<(), String> {
        let chunks_set: FxHashSet<_> = chunks.into_iter().map(|c| *c.borrow()).collect();
        if chunks_set.is_empty() {
            return Ok(());
        }
        let mut entities_to_remove = Vec::new();

        self.entities.rcu(|current_entities| {
            let mut new_entities = (**current_entities).clone();
            new_entities.retain(|entity| {
                let base_entity = entity.get_entity();
                let pos = base_entity.chunk_pos.load();
                if chunks_set.contains(&pos) {
                    entities_to_remove.push(entity.clone());
                    false
                } else {
                    true
                }
            });
            new_entities
        });

        for entity in entities_to_remove {
            self.entity_tracker.remove_entity(entity.as_ref(), self);
            if let Err(error) = self.save_entity(&entity).await {
                // The entity was removed from the live index before saving so
                // it cannot be ticked while its chunk is being evicted. Restore
                // that ownership on failure; silently dropping it would make a
                // subsequent reload impossible even though the process is still
                // alive.
                self.entity_tracker.add_entity(&entity, self);
                self.entities.rcu(|current_entities| {
                    if current_entities.iter().any(|live| {
                        live.get_entity().entity_uuid == entity.get_entity().entity_uuid
                    }) {
                        return (**current_entities).clone();
                    }
                    let mut restored = (**current_entities).clone();
                    restored.push(entity.clone());
                    restored
                });
                return Err(error);
            }
            self.spawn_state.load().remove_entity(self, entity.as_ref());
        }

        for chunk_pos in &chunks_set {
            self.save_block_entities(*chunk_pos);
            self.block_entities.remove(chunk_pos);
        }
        Ok(())
    }

    pub(crate) fn set_block_breaking(
        &self,
        from: &Entity,
        location: BlockPos,
        progress: BlockBreakingProgress,
    ) {
        let chunk_pos = location.chunk_position(); // pumpkin's BlockPos already has this method
        let (stage, bedrock_event) = match progress {
            BlockBreakingProgress::Start { stage, speed } => (
                stage,
                Some((
                    LevelEvent::BlockStartBreak,
                    bedrock_block_breaking_rate(speed),
                )),
            ),
            BlockBreakingProgress::Update { stage, speed } => (
                stage,
                speed.map(|speed| {
                    (
                        LevelEvent::BlockUpdateBreak,
                        bedrock_block_breaking_rate(speed),
                    )
                }),
            ),
            BlockBreakingProgress::Stop => (-1, Some((LevelEvent::BlockStopBreak, 0))),
        };
        let je_packet = CSetBlockDestroyStage::new(from.entity_id.into(), location, stage as i8);

        if let Some((event_id, data)) = bedrock_event {
            let be_packet = CLevelEvent {
                event_id: VarInt(event_id as i32),
                position: Vector3::new(
                    location.0.x as f32,
                    location.0.y as f32,
                    location.0.z as f32,
                ),
                data: VarInt(data),
            };

            if let Some(player) = self.get_player_by_uuid(from.entity_uuid)
                && let ClientPlatform::Bedrock(client) = player.client.as_ref()
                && let Ok(packet_data) = client.serialize_packet(&be_packet)
            {
                client.try_enqueue_packet(packet_data);
            }

            self.broadcast_to_chunk_except_editioned(
                chunk_pos,
                &[from.entity_uuid],
                &je_packet,
                &be_packet,
            );
        } else {
            self.broadcast_to_chunk_except(chunk_pos, &[from.entity_uuid], &je_packet);
        }
    }

    #[expect(clippy::too_many_lines)]
    pub fn set_block_state(
        self: &Arc<Self>,
        position: &BlockPos,
        block_state_id: BlockStateId,
        flags: BlockFlags,
    ) -> BlockStateId {
        if !self.is_in_build_limit(*position) {
            return Block::AIR.default_state.id;
        }

        let (chunk_coordinate, relative) = position.chunk_and_chunk_relative_position();
        let replaced_block_state_id = self
            .level
            .read_chunk_sync(&chunk_coordinate, |chunk| {
                let replaced_block_state_id = chunk.set_block_absolute_y(
                    relative.x as usize,
                    relative.y,
                    relative.z as usize,
                    block_state_id,
                );
                // Mark chunk dirty if it isn't already
                if replaced_block_state_id != block_state_id && !chunk.is_dirty() {
                    chunk.mark_dirty(true);
                }
                replaced_block_state_id
            })
            .unwrap_or(Block::AIR.default_state.id);

        if !flags.contains(BlockFlags::FORCE_STATE) && replaced_block_state_id == block_state_id {
            return block_state_id;
        }

        let old_block = Block::from_state_id(replaced_block_state_id);
        let new_block = Block::from_state_id(block_state_id);
        let is_new_block = old_block != new_block;
        let block_moved = flags.contains(BlockFlags::MOVED);

        if is_new_block && old_block.default_state.block_entity_type != u16::MAX {
            if let Some(entity) = self
                .block_entities
                .get(&chunk_coordinate)
                .and_then(|entities| entities.get(position).cloned())
                && !flags.contains(BlockFlags::SKIP_BLOCK_ENTITY_REPLACED_CALLBACK)
            {
                entity.on_block_replaced(self, position);
            }
            self.remove_block_entity(position);
        }

        if is_new_block && (flags.contains(BlockFlags::NOTIFY_NEIGHBORS) || block_moved) {
            self.block_registry.on_state_replaced(
                self,
                old_block,
                position,
                replaced_block_state_id,
                block_moved,
            );
        }

        if !flags.contains(BlockFlags::SKIP_BLOCK_ADDED_CALLBACK) && is_new_block {
            self.block_registry.on_placed(
                self,
                new_block,
                block_state_id,
                position,
                replaced_block_state_id,
                block_moved,
            );
            let new_fluid = self.get_fluid(position);
            self.block_registry.on_placed_fluid(
                self,
                new_fluid,
                block_state_id,
                position,
                replaced_block_state_id,
                block_moved,
            );
        }

        // Level.java setBlock
        if self.get_block_state_id(position) == block_state_id {
            if flags.contains(BlockFlags::NOTIFY_LISTENERS) {
                self.unsent_block_changes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(*position, block_state_id);
            }

            if flags.contains(BlockFlags::NOTIFY_NEIGHBORS) {
                self.update_neighbors_at(position, old_block, None);
                if block_state_id.has_analog_output_signal() {
                    self.update_neighbour_for_output_signal(position, new_block);
                }
            }

            if !flags.contains(BlockFlags::MOVED) {
                let mut neighbour_update_flags = flags;
                neighbour_update_flags.remove(BlockFlags::NOTIFY_NEIGHBORS);
                neighbour_update_flags.remove(BlockFlags::SKIP_REDSTONE_WIRE_STATE_REPLACEMENT);
                self.block_registry.prepare(
                    self,
                    position,
                    old_block,
                    replaced_block_state_id,
                    neighbour_update_flags,
                );
                self.block_registry
                    .update_neighbors(self, position, neighbour_update_flags);
                self.block_registry.prepare(
                    self,
                    position,
                    new_block,
                    block_state_id,
                    neighbour_update_flags,
                );
            }

            self.villager_poi
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .update_block(*position, new_block);

            if is_new_block {
                let mut poi = self
                    .portal_poi
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if villager_poi::profession_for_block(old_block).is_some() {
                    poi.remove(position);
                }
                if let Some(poi_type) = villager_poi::poi_type_for_block(new_block) {
                    poi.add_with_free_tickets(*position, poi_type, 1);
                }
            }
        }

        let old_state = replaced_block_state_id.to_state();
        let new_state = block_state_id.to_state();
        if pumpkin_world::lighting::LightEngine::has_different_light_properties(
            old_state, new_state,
        ) {
            self.level
                .light_engine
                .update_lighting_at(&self.level, *position);
        }

        replaced_block_state_id
    }

    pub fn break_block(
        self: &Arc<Self>,
        position: &BlockPos,
        cause: Option<&Arc<Player>>,
        flags: BlockFlags,
    ) -> Option<BlockStateId> {
        if let Some(player) = cause
            && self.is_in_spawn_protection(player, position)
        {
            player.send_system_message(&TextComponent::translate_cross(
                pumpkin_data::translation::java::BUILD_SPAWN_PROTECTION,
                pumpkin_data::translation::java::BUILD_SPAWN_PROTECTION,
                [TextComponent::text(player.gameprofile.name.clone())],
            ));
            return None;
        }

        let (broken_block, broken_block_state) = self.get_block_and_state(position);
        if broken_block_state.is_air() {
            return None;
        }

        let mut flags = flags;
        if flags.contains(BlockFlags::SKIP_DROPS)
            && cause.is_some_and(|p| p.gamemode.load() == pumpkin_util::GameMode::Creative)
            && self
                .get_block_entity(position)
                .is_some_and(|entity| entity.drops_for_creative_player())
        {
            flags.remove(BlockFlags::SKIP_DROPS);
        }

        let mut event = BlockBreakEvent::new(
            cause.cloned(),
            broken_block,
            *position,
            0,
            !flags.contains(BlockFlags::SKIP_DROPS),
        );
        if let Some(server) = self.server.upgrade() {
            server.plugin_manager.fire_blocking(&server, &mut event);
        }
        if event.cancelled {
            return None;
        }

        if event.drop {
            flags.remove(BlockFlags::SKIP_DROPS);
        } else {
            flags.insert(BlockFlags::SKIP_DROPS);
        }

        if !flags.contains(BlockFlags::SKIP_DROPS) {
            let tool = cause.as_ref().and_then(|p| {
                let item = p.inventory().held_item();
                if item.is_empty() { None } else { Some(item) }
            });
            let params = crate::world::loot::LootContextParameters {
                tool,
                block_state: Some(broken_block_state),
                position: Some(position.to_f64()),
                killed_by_player: Some(cause.is_some()),
                ..Default::default()
            };
            crate::block::drop_loot(self, broken_block, position, true, &params);
        }

        let new_state_id = if broken_block.is_waterlogged(broken_block_state.id) {
            Block::WATER.default_state.id
        } else {
            Block::AIR.default_state.id
        };

        let broken_state_id = self.set_block_state(position, new_state_id, flags);
        let broken_block = Block::from_state_id(broken_state_id);
        if !broken_block.is_air()
            && broken_state_id != new_state_id
            && broken_block != &Block::FIRE
            && broken_block != &Block::SOUL_FIRE
        {
            let je_packet = CWorldEvent::new(
                WorldEvent::ParticlesDestroyBlock as i32,
                *position,
                broken_state_id.as_u16().into(),
                false,
            );
            let be_packet = CLevelEvent {
                event_id: VarInt(LevelEvent::ParticlesDestroyBlock as i32),
                position: position.to_centered_f64().to_f32_lossy(),
                data: VarInt(BlockState::to_be_network_id(broken_state_id) as i32),
            };
            let chunk_pos = position.chunk_position();
            if let Some(player) = cause {
                // Java predicts its own break effect; Bedrock needs the server event.
                if let ClientPlatform::Bedrock(client) = player.client.as_ref() {
                    client.try_enqueue_client_packet(&be_packet);
                }
                self.broadcast_to_chunk_except_editioned(
                    chunk_pos,
                    &[player.get_entity().entity_uuid],
                    &je_packet,
                    &be_packet,
                );
            } else {
                self.broadcast_to_chunk_editioned(chunk_pos, &je_packet, &be_packet);
            }
        }

        Some(broken_state_id)
    }

    #[must_use]
    pub const fn environment_attributes(&self) -> EnvironmentAttributes<'_> {
        EnvironmentAttributes::new(self)
    }

    #[must_use]
    pub fn get_sky_darken(&self) -> i32 {
        let sky_light_level = self.environment_attributes().get_dimension_value_f32(
            pumpkin_data::environment_attribute::EnvironmentAttribute::GameplaySkyLightLevel,
        );
        (15.0 - sky_light_level).clamp(0.0, 15.0) as i32
    }

    #[must_use]
    pub fn is_bright_outside(&self) -> bool {
        !self.dimension.has_fixed_time && self.get_sky_darken() < 4
    }

    #[must_use]
    pub fn is_dark_outside(&self) -> bool {
        !self.dimension.has_fixed_time && !self.is_bright_outside()
    }

    /// Checks if daylight burns undead monsters (`EnvironmentAttributes.MONSTERS_BURN`).
    #[must_use]
    pub fn monsters_burn(&self, pos: &BlockPos) -> bool {
        self.environment_attributes().get_value_bool(
            pumpkin_data::environment_attribute::EnvironmentAttribute::GameplayMonstersBurn,
            pos,
        )
    }

    /// Checks if bees should stay inside beehives/nests (`EnvironmentAttributes.BEES_STAY_IN_HIVE`).
    #[must_use]
    pub fn bees_stay_in_hive(&self, pos: &BlockPos) -> bool {
        self.environment_attributes().get_value_bool(
            pumpkin_data::environment_attribute::EnvironmentAttribute::GameplayBeesStayInHive,
            pos,
        )
    }

    /// Checks if a creaking heart is active (`EnvironmentAttributes.CREAKING_ACTIVE`).
    #[must_use]
    pub fn creaking_active(&self, pos: &BlockPos) -> bool {
        self.environment_attributes().get_value_bool(
            pumpkin_data::environment_attribute::EnvironmentAttribute::GameplayCreakingActive,
            pos,
        )
    }

    /// Checks if an eyeblossom flower should be open (`EnvironmentAttributes.EYEBLOSSOM_OPEN`).
    #[must_use]
    pub fn eyeblossom_open(&self, pos: &BlockPos) -> Option<bool> {
        self.environment_attributes().get_value_tri_state(
            pumpkin_data::environment_attribute::EnvironmentAttribute::GameplayEyeblossomOpen,
            pos,
        )
    }

    #[must_use]
    pub fn get_effective_sky_brightness(&self, pos: &BlockPos) -> i32 {
        let sky_light = self.get_sky_light_level(pos) as i32;
        sky_light - self.get_sky_darken()
    }

    #[must_use]
    pub fn get_sun_angle(&self, pos: &BlockPos) -> f32 {
        let sun_angle_deg = self.environment_attributes().get_value_f32(
            pumpkin_data::environment_attribute::EnvironmentAttribute::VisualSunAngle,
            pos,
        );
        sun_angle_deg * (std::f32::consts::PI / 180.0)
    }

    #[must_use]
    pub fn get_moon_phase(&self) -> MoonPhase {
        self.environment_attributes()
            .get_dimension_value_moon_phase()
    }

    #[must_use]
    pub fn can_pillager_patrol_spawn(&self, pos: &BlockPos) -> bool {
        self.environment_attributes().get_value_bool(
            pumpkin_data::environment_attribute::EnvironmentAttribute::GameplayCanPillagerPatrolSpawn,
            pos,
        )
    }

    #[must_use]
    pub fn surface_slime_spawn_chance(&self, pos: &BlockPos) -> f32 {
        self.environment_attributes().get_value_f32(
            pumpkin_data::environment_attribute::EnvironmentAttribute::GameplaySurfaceSlimeSpawnChance,
            pos,
        )
    }

    #[must_use]
    pub fn villager_activity(&self, pos: &BlockPos, baby: bool) -> Activity {
        self.environment_attributes().get_value_activity(baby, pos)
    }

    pub fn get_raw_brightness(&self, pos: &BlockPos, sky_darken: u8) -> u8 {
        let sky_light = self.get_sky_light_level(pos).saturating_sub(sky_darken);
        let block_light = self.get_block_light_level(pos).unwrap_or(0);
        sky_light.max(block_light)
    }

    pub fn get_max_local_raw_brightness(&self, pos: &BlockPos) -> u8 {
        self.get_raw_brightness(pos, self.get_sky_darken() as u8)
    }

    pub fn get_block_light_level(&self, position: &BlockPos) -> Option<u8> {
        self.level
            .light_engine
            .get_block_light_level(&self.level, position)
    }

    pub fn get_sky_light_level(&self, position: &BlockPos) -> u8 {
        self.level
            .light_engine
            .get_sky_light_level(&self.level, position)
    }

    #[must_use]
    pub fn can_see_sky(&self, position: &BlockPos) -> bool {
        position.0.y >= self.dimension.min_y
            && position.0.y < self.dimension.min_y + self.dimension.height
            && self.get_sky_light_level(position) >= MAX_LIGHT_LEVEL
    }

    pub fn set_block_light_level(&self, position: &BlockPos, light_level: u8) {
        let _ = self
            .level
            .light_engine
            .set_block_light_level(&self.level, position, light_level);
    }

    pub fn set_sky_light_level(&self, position: &BlockPos, light_level: u8) {
        let _ = self
            .level
            .light_engine
            .set_sky_light_level(&self.level, position, light_level);
    }

    pub fn get_biome(&self, position: &BlockPos) -> &'static Biome {
        let chunk_pos = position.chunk_position();
        if let Some(chunk) = self.level.loaded_chunks.get(&chunk_pos) {
            let id = chunk
                .section
                .get_rough_biome_absolute_y(
                    (position.0.x & 15) as usize,
                    position.0.y,
                    (position.0.z & 15) as usize,
                )
                .unwrap_or(0);
            Biome::from_id(id).unwrap_or(&Biome::PLAINS)
        } else {
            &Biome::PLAINS
        }
    }

    pub fn schedule_block_tick(
        &self,
        block: &Block,
        block_pos: BlockPos,
        delay: u8,
        priority: TickPriority,
    ) {
        self.level
            .schedule_block_tick(block, block_pos, delay, priority);
    }

    pub fn schedule_fluid_tick(
        &self,
        fluid: &Fluid,
        block_pos: BlockPos,
        delay: u8,
        priority: TickPriority,
    ) {
        self.level
            .schedule_fluid_tick(fluid, block_pos, delay, priority);
    }

    pub fn is_block_tick_scheduled(&self, block_pos: &BlockPos, block: &Block) -> bool {
        self.level.is_block_tick_scheduled(block_pos, block)
    }

    pub fn is_fluid_tick_scheduled(&self, block_pos: &BlockPos, fluid: &Fluid) -> bool {
        self.level.is_fluid_tick_scheduled(block_pos, fluid)
    }

    /// Close container screens for all players who have a container open at the given block position.
    pub fn close_container_screens_at(&self, position: &BlockPos) {
        let players = self.players.load();
        for player in players.iter() {
            if player.open_container_pos.load() == Some(*position) {
                player.close_handled_screen();
            }
        }
    }

    pub fn drop_stack(self: &Arc<Self>, pos: &BlockPos, stack: ItemStack) {
        if stack.is_empty() {
            return;
        }

        let half_height = f64::from(EntityType::ITEM.dimension[1]) / 2.0;
        let spawn_pos = {
            let mut r = rand::rng();
            Vector3::new(
                f64::from(pos.0.x) + 0.5 + r.random_range(-0.25..0.25),
                f64::from(pos.0.y) + 0.5 + r.random_range(-0.25..0.25) - half_height,
                f64::from(pos.0.z) + 0.5 + r.random_range(-0.25..0.25),
            )
        };

        let entity = Entity::new(self.clone(), spawn_pos, &EntityType::ITEM);
        let mut item_event = crate::plugin::api::events::entity::item_spawn::ItemSpawnEvent::new(
            entity.entity_id,
            spawn_pos,
            stack.item.registry_key.to_string(),
        );
        if let Some(server) = self.server.upgrade() {
            server
                .plugin_manager
                .fire_blocking(&server, &mut item_event);
        }
        if item_event.cancelled {
            return;
        }

        let item_entity = Arc::new(ItemEntity::new(entity, stack));
        self.spawn_entity(item_entity);
    }

    pub fn drop_stack_from_face(
        self: &Arc<Self>,
        pos: &BlockPos,
        face: BlockDirection,
        stack: ItemStack,
    ) {
        if stack.is_empty() {
            return;
        }

        let offset = face.to_offset();
        let step_x = offset.x;
        let step_y = offset.y;
        let step_z = offset.z;

        let half_width = f64::from(EntityType::ITEM.dimension[0]) / 2.0;
        let half_height = f64::from(EntityType::ITEM.dimension[1]) / 2.0;

        let (spawn_pos, velocity) = {
            let mut r = rand::rng();
            let x = f64::from(pos.0.x)
                + 0.5
                + if step_x == 0 {
                    r.random_range(-0.25..0.25)
                } else {
                    f64::from(step_x) * (0.5 + half_width)
                };
            let y = f64::from(pos.0.y)
                + 0.5
                + if step_y == 0 {
                    r.random_range(-0.25..0.25)
                } else {
                    f64::from(step_y) * (0.5 + half_height)
                }
                - half_height;
            let z = f64::from(pos.0.z)
                + 0.5
                + if step_z == 0 {
                    r.random_range(-0.25..0.25)
                } else {
                    f64::from(step_z) * (0.5 + half_width)
                };

            let delta_x = if step_x == 0 {
                r.random_range(-0.1..0.1)
            } else {
                f64::from(step_x) * 0.1
            };
            let delta_y = if step_y == 0 {
                r.random_range(0.0..0.1)
            } else {
                f64::from(step_y) * 0.1 + 0.1
            };
            let delta_z = if step_z == 0 {
                r.random_range(-0.1..0.1)
            } else {
                f64::from(step_z) * 0.1
            };

            (
                Vector3::new(x, y, z),
                Vector3::new(delta_x, delta_y, delta_z),
            )
        };

        let entity = Entity::new(self.clone(), spawn_pos, &EntityType::ITEM);
        let mut item_event = crate::plugin::api::events::entity::item_spawn::ItemSpawnEvent::new(
            entity.entity_id,
            spawn_pos,
            stack.item.registry_key.to_string(),
        );
        if let Some(server) = self.server.upgrade() {
            server
                .plugin_manager
                .fire_blocking(&server, &mut item_event);
        }
        if item_event.cancelled {
            return;
        }

        let item_entity = Arc::new(ItemEntity::new_with_velocity(entity, stack, velocity, 10));
        self.spawn_entity(item_entity);
    }

    pub fn strike_lightning(self: &Arc<Self>, pos: Vector3<f64>, effect_only: bool) {
        use pumpkin_data::entity::EntityType;
        use uuid::Uuid;
        let server_ref = self.server.upgrade();
        if let Some(server_ref) = server_ref {
            let mut event =
                crate::plugin::api::events::world::lightning_strike::LightningStrikeEvent::new(
                    pos,
                    effect_only,
                );
            server_ref
                .plugin_manager
                .fire_blocking(&server_ref, &mut event);
            if event.cancelled {
                return;
            }
        }

        let lightning = crate::entity::r#type::from_type(
            &EntityType::LIGHTNING_BOLT,
            pos,
            self,
            Uuid::new_v4(),
        );

        if let Some(bolt) = lightning
            .cast_any()
            .downcast_ref::<crate::entity::lightning::LightningBoltEntity>()
        {
            bolt.set_visual_only(effect_only);
        }

        self.spawn_entity(lightning);
    }

    /* ItemScatterer.java */
    pub fn scatter_inventory(
        self: &Arc<Self>,
        position: &BlockPos,
        inventory: &Arc<dyn Inventory>,
    ) {
        for i in 0..inventory.size() {
            self.scatter_stack(
                f64::from(position.0.x),
                f64::from(position.0.y),
                f64::from(position.0.z),
                inventory.remove_stack(i),
            );
        }
    }
    pub fn scatter_stack(self: &Arc<Self>, x: f64, y: f64, z: f64, mut stack: ItemStack) {
        const TRIANGULAR_DEVIATION: f64 = 0.114_850_001_711_398_36;

        const XZ_MODE: f64 = 0.0;
        const Y_MODE: f64 = 0.2;

        let width = f64::from(EntityType::ITEM.dimension[0]);
        let half_width = width / 2.0;
        let spawn_area = 1.0 - width;

        let mut rng = Xoroshiro::from_seed(get_seed());

        // TODO: Use world random here: world.random.nextDouble()
        let x = rng.next_f64().mul_add(spawn_area, x.floor()) + half_width;
        let y = rng.next_f64().mul_add(spawn_area, y.floor());
        let z = rng.next_f64().mul_add(spawn_area, z.floor()) + half_width;

        while !stack.is_empty() {
            let item = stack.split((rng.next_bounded_i32(21) + 10) as u8);
            let velocity = Vector3::new(
                rng.next_triangular(XZ_MODE, TRIANGULAR_DEVIATION),
                rng.next_triangular(Y_MODE, TRIANGULAR_DEVIATION),
                rng.next_triangular(XZ_MODE, TRIANGULAR_DEVIATION),
            );

            let entity = Entity::new(self.clone(), Vector3::new(x, y, z), &EntityType::ITEM);
            let entity = Arc::new(ItemEntity::new_with_velocity(entity, item, velocity, 10));
            self.spawn_entity(entity);
        }
    }
    /* End ItemScatterer.java */

    /// Updates neighboring blocks of a block with a specified source block
    pub fn update_neighbors_at(
        self: &Arc<Self>,
        block_pos: &BlockPos,
        source_block: &Block,
        except: Option<BlockDirection>,
    ) {
        for direction in BlockDirection::update_order() {
            if except.is_some_and(|d| d == direction) {
                continue;
            }

            let neighbor_pos = block_pos.offset(direction.to_offset());
            let (neighbor_block, neighbor_fluid) = self.get_block_and_fluid(&neighbor_pos);

            let mut event =
                crate::plugin::api::events::block::block_physics::BlockPhysicsEvent::new(
                    neighbor_pos,
                    *block_pos,
                );
            if let Some(server) = self.server.upgrade() {
                server.plugin_manager.fire_blocking(&server, &mut event);
            }
            if event.cancelled {
                continue;
            }

            if let Some(neighbor_pumpkin_block) =
                self.block_registry.get_pumpkin_block(neighbor_block.id)
            {
                neighbor_pumpkin_block.on_neighbor_update(OnNeighborUpdateArgs {
                    world: self,
                    block: neighbor_block,
                    position: &neighbor_pos,
                    source_block,
                    notify: false,
                });
            }

            if let Some(neighbor_pumpkin_fluid) =
                self.block_registry.get_pumpkin_fluid(neighbor_fluid.id)
            {
                neighbor_pumpkin_fluid.on_neighbor_update(
                    self,
                    neighbor_fluid,
                    &neighbor_pos,
                    false,
                );
            }
        }
    }

    /// Updates neighboring blocks of a block
    pub fn update_neighbors(
        self: &Arc<Self>,
        block_pos: &BlockPos,
        except: Option<BlockDirection>,
    ) {
        let source_block = self.get_block(block_pos);
        self.update_neighbors_at(block_pos, source_block, except);
    }

    pub fn update_neighbor(self: &Arc<Self>, neighbor_block_pos: &BlockPos, source_block: &Block) {
        let neighbor_block = self.get_block(neighbor_block_pos);

        let mut event = crate::plugin::api::events::block::block_physics::BlockPhysicsEvent::new(
            *neighbor_block_pos,
            *neighbor_block_pos,
        );
        if let Some(server) = self.server.upgrade() {
            server.plugin_manager.fire_blocking(&server, &mut event);
        }
        if event.cancelled {
            return;
        }

        if let Some(neighbor_pumpkin_block) =
            self.block_registry.get_pumpkin_block(neighbor_block.id)
        {
            neighbor_pumpkin_block.on_neighbor_update(OnNeighborUpdateArgs {
                world: self,
                block: neighbor_block,
                position: neighbor_block_pos,
                source_block,
                notify: false,
            });
        }
    }

    pub fn update_neighbour_for_output_signal(
        self: &Arc<Self>,
        pos: &BlockPos,
        changed_block: &Block,
    ) {
        for direction in BlockDirection::horizontal() {
            let mut relative_pos = pos.offset(direction.to_offset());
            if self.is_loaded(&relative_pos) {
                let state = self.get_block_state(&relative_pos);
                if state.id.to_block() == &Block::COMPARATOR {
                    self.update_neighbor(&relative_pos, changed_block);
                } else if state.is_solid_block() {
                    relative_pos = relative_pos.offset(direction.to_offset());
                    if self.is_loaded(&relative_pos) {
                        let second_state = self.get_block_state(&relative_pos);
                        if second_state.id.to_block() == &Block::COMPARATOR {
                            self.update_neighbor(&relative_pos, changed_block);
                        }
                    }
                }
            }
        }
    }

    pub fn update_from_neighbor_shapes(
        self: &Arc<Self>,
        state_id: BlockStateId,
        pos: &BlockPos,
    ) -> BlockStateId {
        let mut current_state_id = state_id;
        let block = Block::from_state_id(state_id);
        for direction in BlockDirection::all() {
            let neighbor_pos = pos.offset(direction.to_offset());
            let neighbor_state_id = self.get_block_state_id(&neighbor_pos);
            current_state_id = self.block_registry.get_state_for_neighbor_update(
                self,
                block,
                current_state_id,
                pos,
                direction,
                &neighbor_pos,
                neighbor_state_id,
            );
        }
        current_state_id
    }

    pub fn replace_with_state_for_neighbor_update(
        self: &Arc<Self>,
        block_pos: &BlockPos,
        direction: BlockDirection,
        flags: BlockFlags,
    ) {
        let (block, block_state_id) = self.get_block_and_state_id(block_pos);

        if flags.contains(BlockFlags::SKIP_REDSTONE_WIRE_STATE_REPLACEMENT)
            && *block == Block::REDSTONE_WIRE
        {
            return;
        }

        let neighbor_pos = block_pos.offset(direction.to_offset());
        let neighbor_state_id = self.get_block_state_id(&neighbor_pos);

        let new_state_id = self.block_registry.get_state_for_neighbor_update(
            self,
            block,
            block_state_id,
            block_pos,
            direction,
            &neighbor_pos,
            neighbor_state_id,
        );

        if new_state_id != block_state_id {
            if is_air(new_state_id) {
                self.break_block(block_pos, None, flags | BlockFlags::NOTIFY_ALL);
            } else {
                self.set_block_state(block_pos, new_state_id, flags);
            }
        }
    }

    /// Returns whether monsters can be spawned in the world
    pub fn should_spawn_monsters(&self) -> bool {
        let level_data = self.level_info.load();
        level_data.game_rules.spawn_mobs
            && level_data.game_rules.spawn_monsters
            && level_data.difficulty != Difficulty::Peaceful
    }

    #[must_use]

    /// Clips the segment against the outline shapes of `state`. A shapeless block,
    /// air above all, cannot be hit.

    #[allow(clippy::too_many_lines)]

    /// Returns the closest entity the segment from `start` to `end` hits, or
    /// `None`. Convenience wrapper over [`Self::ray_trace_entities`].

    /// Traces the block grid from `start_pos` to `end_pos` (vanilla
    /// `Block.clip` semantics) and returns the first block the ray actually
    /// passes through whose outline collides and whose `hit_check` returns
    /// true, together with the direction reported for that hit. The start
    /// block is tested like any other; since the ray begins inside it, the
    /// reported direction there is a fallback rather than a true entry face.
    /// Returns `None` when nothing is hit or the ray starts and ends in the
    /// same block.

    /// Broadcasts a packet to all players who currently have the target chunk loaded.

    /// Broadcasts a packet to chunk watchers, excluding specific players.

    pub fn emit_game_event(&self, event_key: impl Into<String>, position: Vector3<f64>) {
        self.emit_game_event_with_source(event_key, position, None, None);
    }

    pub fn emit_game_event_with_source(
        &self,
        event_key: impl Into<String>,
        position: Vector3<f64>,
        source: Option<crate::world::SculkEventSource>,
        affected_state: Option<pumpkin_data::BlockStateId>,
    ) {
        self.emit_game_event_internal(event_key.into(), position, source, affected_state);
    }

    pub(crate) fn emit_entity_die(
        self: &Arc<Self>,
        position: Vector3<f64>,
        source: crate::world::SculkEventSource,
        experience_eligible: bool,
        experience_reward: u32,
    ) -> bool {
        let mut event = crate::plugin::api::events::world::generic_game::GenericGameEvent::new(
            "entity_die".to_owned(),
            position,
        );
        if let Some(server) = self.server.upgrade() {
            server.plugin_manager.fire_blocking(&server, &mut event);
        }
        if event.cancelled {
            return false;
        }
        let Some(game_event) = pumpkin_data::game_event::GameEvent::from_name(&event.event_key)
        else {
            return false;
        };
        let catalyst_consumed = game_event == pumpkin_data::game_event::GameEvent::EntityDie
            && self.dispatch_catalyst_death(
                game_event,
                true,
                event.position,
                experience_eligible,
                experience_reward,
            );
        self.dispatch_sculk_vibration(game_event, event.position, Some(source), None);
        catalyst_consumed
    }

    fn emit_game_event_internal(
        &self,
        event_key: String,
        position: Vector3<f64>,
        source: Option<crate::world::SculkEventSource>,
        affected_state: Option<pumpkin_data::BlockStateId>,
    ) -> (bool, bool) {
        let mut event = crate::plugin::api::events::world::generic_game::GenericGameEvent::new(
            event_key, position,
        );
        if let Some(server) = self.server.upgrade() {
            server.plugin_manager.fire_blocking(&server, &mut event);
        }
        if event.cancelled {
            return (true, false);
        }
        let catalyst_consumed = pumpkin_data::game_event::GameEvent::from_name(&event.event_key)
            .is_some_and(|game_event| {
                self.dispatch_sculk_vibration(game_event, event.position, source, affected_state)
            });
        (false, catalyst_consumed)
    }

    fn vibration_occluded(&self, event: Vector3<f64>, listener: Vector3<f64>) -> bool {
        use pumpkin_data::tag::Taggable;
        let event_block =
            pumpkin_util::math::position::BlockPos::floored(event.x, event.y, event.z);
        let event_center = Vector3::new(
            f64::from(event_block.0.x) + 0.5,
            f64::from(event_block.0.y) + 0.5,
            f64::from(event_block.0.z) + 0.5,
        );
        [
            (1.0, 0.0, 0.0),
            (-1.0, 0.0, 0.0),
            (0.0, 1.0, 0.0),
            (0.0, -1.0, 0.0),
            (0.0, 0.0, 1.0),
            (0.0, 0.0, -1.0),
        ]
        .into_iter()
        .all(|(nx, ny, nz)| {
            let start = Vector3::new(
                event_center.x + nx * 9.999_999_747_378_752e-6,
                event_center.y + ny * 9.999_999_747_378_752e-6,
                event_center.z + nz * 9.999_999_747_378_752e-6,
            );
            traverse_vibration_blocks(start, listener, |block| {
                self.get_block_state_if_loaded(&block).is_some_and(|state| {
                    Block::from_state_id(state.id)
                        .is_tagged_with("minecraft:occludes_vibration_signals")
                        .unwrap_or(false)
                })
            })
        })
    }

    pub(crate) fn sculk_source_for_vibration(
        &self,
        uuid: uuid::Uuid,
        projectile_owner: Option<uuid::Uuid>,
    ) -> Option<SculkEventSource> {
        let entity = self.get_entity_by_uuid(uuid)?;
        let player = entity.get_player();
        Some(SculkEventSource {
            uuid,
            projectile_owner,
            spectator: player.is_some_and(|player| player.is_spectator()),
            sneaking: entity.get_entity().is_sneaking(),
            dampens_vibrations: entity.dampens_vibrations(),
        })
    }

    fn dispatch_sculk_vibration(
        &self,
        event: pumpkin_data::game_event::GameEvent,
        position: Vector3<f64>,
        source: Option<SculkEventSource>,
        affected_state: Option<pumpkin_data::BlockStateId>,
    ) -> bool {
        use crate::block::blocks::redstone::sculk_sensor::{
            VibrationCandidate, select_vibration, vibration_frequency,
        };
        use crate::block::entities::{
            calibrated_sculk_sensor::CalibratedSculkSensorBlockEntity,
            sculk_sensor::{PendingVibration, SculkSensorBlockEntity},
            sculk_shrieker::SculkShriekerBlockEntity,
        };
        use pumpkin_data::tag::Taggable;
        let catalyst_consumed = false;
        let (sensor_listens, shrieker_listens) = sculk_event_listeners(event);
        if !sensor_listens && !shrieker_listens {
            return catalyst_consumed;
        }
        let frequency = vibration_frequency(event);
        if sensor_listens && frequency == 0 && !shrieker_listens {
            return catalyst_consumed;
        }
        let ignore_sneaking = pumpkin_data::tag::get_tag_values(
            pumpkin_data::tag::RegistryKey::GameEvent,
            "minecraft:ignore_vibrations_sneaking",
        )
        .is_some_and(|events| events.contains(&event.name()));
        if source
            .is_some_and(|s| s.spectator || (s.sneaking && ignore_sneaking) || s.dampens_vibrations)
        {
            return catalyst_consumed;
        }
        if affected_state.is_some_and(|state| {
            Block::from_state_id(state)
                .is_tagged_with("minecraft:dampens_vibrations")
                .unwrap_or(false)
        }) {
            return catalyst_consumed;
        }
        let event_block =
            pumpkin_util::math::position::BlockPos::floored(position.x, position.y, position.z);
        let chunk_min_x = (event_block.0.x - 16) >> 4;
        let chunk_max_x = (event_block.0.x + 16) >> 4;
        let chunk_min_z = (event_block.0.z - 16) >> 4;
        let chunk_max_z = (event_block.0.z + 16) >> 4;
        for cx in chunk_min_x..=chunk_max_x {
            for cz in chunk_min_z..=chunk_max_z {
                let Some(entities) = self
                    .block_entities
                    .get(&pumpkin_util::math::vector2::Vector2::new(cx, cz))
                else {
                    continue;
                };
                for (pos, be) in entities.iter() {
                    let Some((selector, current, sensor_radius, is_shrieker)) = be
                        .as_any()
                        .downcast_ref::<SculkSensorBlockEntity>()
                        .map(|e| (&e.selector_vibration, &e.pending_vibration, 8.0, false))
                        .or_else(|| {
                            be.as_any()
                                .downcast_ref::<CalibratedSculkSensorBlockEntity>()
                                .map(|e| (&e.selector_vibration, &e.pending_vibration, 16.0, false))
                        })
                        .or_else(|| {
                            be.as_any()
                                .downcast_ref::<SculkShriekerBlockEntity>()
                                .map(|e| (&e.selector_vibration, &e.pending_vibration, 8.0, true))
                        })
                    else {
                        continue;
                    };
                    if is_shrieker != shrieker_listens || (!is_shrieker && !sensor_listens) {
                        continue;
                    }
                    if is_shrieker
                        && !source.is_some_and(|source| {
                            let projectile_owner = source
                                .projectile_owner
                                .and_then(|owner| self.get_player_by_uuid(owner))
                                .is_some();
                            let direct_player = self.get_player_by_uuid(source.uuid).is_some();
                            let source_entity = self.get_entity_by_uuid(source.uuid);
                            let item_owner = source_entity.as_ref().is_some_and(|entity| {
                                entity
                                    .get_item_entity()
                                    .and_then(ItemEntity::get_owner)
                                    .and_then(|owner| self.get_player_by_uuid(owner))
                                    .is_some()
                            });
                            let passenger = source_entity.is_some_and(|entity| {
                                entity
                                    .get_entity()
                                    .passengers
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .first()
                                    .and_then(|passenger| passenger.get_player())
                                    .and_then(|passenger| {
                                        self.get_player_by_uuid(passenger.gameprofile.id)
                                    })
                                    .is_some()
                            });
                            shrieker_source_eligible(
                                projectile_owner,
                                direct_player,
                                item_owner,
                                passenger,
                            )
                        })
                    {
                        continue;
                    }
                    if !within_sculk_listener_radius(event_block, *pos, sensor_radius as i32) {
                        continue;
                    }
                    if current
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_some()
                    {
                        continue;
                    }
                    let Some(state) = self.get_block_state_if_loaded(pos) else {
                        continue;
                    };
                    let block = Block::from_state_id(state.id);
                    if block.id == pumpkin_data::BlockId::CALIBRATED_SCULK_SENSOR {
                        let props = pumpkin_data::block_properties::CalibratedSculkSensorLikeProperties::from_state_id(state.id);
                        if props.sculk_sensor_phase
                            != pumpkin_data::block_properties::SculkSensorPhase::Inactive
                        {
                            continue;
                        }
                        let facing = match props.facing {
                            pumpkin_data::block_properties::HorizontalFacing::North => {
                                pumpkin_data::BlockDirection::North
                            }
                            pumpkin_data::block_properties::HorizontalFacing::South => {
                                pumpkin_data::BlockDirection::South
                            }
                            pumpkin_data::block_properties::HorizontalFacing::West => {
                                pumpkin_data::BlockDirection::West
                            }
                            pumpkin_data::block_properties::HorizontalFacing::East => {
                                pumpkin_data::BlockDirection::East
                            }
                        };
                        let back_pos = pos.offset(facing.opposite().to_offset());
                        let Some(back_state) = self.get_block_state_if_loaded(&back_pos) else {
                            continue;
                        };
                        let back_block = Block::from_state_id(back_state.id);
                        let calibrated_frequency = self.block_registry.get_weak_redstone_power(
                            back_block,
                            self,
                            &back_pos,
                            back_state,
                            facing.opposite(),
                        );
                        if calibrated_frequency > 0 && calibrated_frequency != frequency as u8 {
                            continue;
                        }
                    } else if block.id == pumpkin_data::BlockId::SCULK_SENSOR {
                        if pumpkin_data::block_properties::SculkSensorLikeProperties::from_state_id(
                            state.id,
                        )
                        .sculk_sensor_phase
                            != pumpkin_data::block_properties::SculkSensorPhase::Inactive
                        {
                            continue;
                        }
                    } else if is_shrieker
                        && pumpkin_data::block_properties::SculkShriekerLikeProperties::from_state_id(
                            state.id,
                        )
                        .shrieking
                    {
                        continue;
                    }
                    let center = Vector3::new(
                        f64::from(pos.0.x) + 0.5,
                        f64::from(pos.0.y) + 0.5,
                        f64::from(pos.0.z) + 0.5,
                    );
                    let distance = (center.x - position.x)
                        .hypot(center.y - position.y)
                        .hypot(center.z - position.z);
                    if self.vibration_occluded(position, center) {
                        continue;
                    }
                    let mut pending = selector
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let candidate = VibrationCandidate {
                        tick: self.get_world_age(),
                        distance,
                        frequency,
                    };
                    let current = pending.map(|v| VibrationCandidate {
                        tick: v.tick,
                        distance: v.distance,
                        frequency: v.frequency,
                    });
                    let chosen = select_vibration(current, candidate);
                    if chosen == Some(candidate) && current != Some(candidate) {
                        *pending = Some(PendingVibration {
                            event,
                            distance,
                            position,
                            delay: distance.floor() as u32,
                            tick: candidate.tick,
                            frequency,
                            source: source.map(|source| source.uuid),
                            projectile_owner: source.and_then(|source| source.projectile_owner),
                        });
                    }
                }
            }
        }
        catalyst_consumed
    }

    fn dispatch_catalyst_death(
        self: &Arc<Self>,
        event: pumpkin_data::game_event::GameEvent,
        has_entity_source: bool,
        event_position: Vector3<f64>,
        experience_eligible: bool,
        experience_reward: u32,
    ) -> bool {
        use crate::block::blocks::sculk::sculk_catalyst::{bloom, hears_entity_death};
        use crate::block::entities::sculk_catalyst::SculkCatalystBlockEntity;
        let event_block = BlockPos::floored(event_position.x, event_position.y, event_position.z);
        let effect_position =
            BlockPos::floored(event_position.x, event_position.y + 0.5, event_position.z);
        let mut nearest: Option<(f64, BlockPos)> = None;
        for cx in ((event_block.0.x - 8) >> 4)..=((event_block.0.x + 8) >> 4) {
            for cz in ((event_block.0.z - 8) >> 4)..=((event_block.0.z + 8) >> 4) {
                let Some(entities) = self.block_entities.get(&Vector2::new(cx, cz)) else {
                    continue;
                };
                for (position, entity) in entities.iter() {
                    if entity
                        .as_any()
                        .downcast_ref::<SculkCatalystBlockEntity>()
                        .is_none()
                    {
                        continue;
                    }
                    let center = position.to_centered_f64();
                    let distance = (center.x - event_position.x)
                        .hypot(center.y - event_position.y)
                        .hypot(center.z - event_position.z);
                    if hears_entity_death(event, has_entity_source, distance)
                        && self
                            .get_block_state_if_loaded(position)
                            .is_some_and(|state| {
                                Block::from_state_id(state.id).id
                                    == pumpkin_data::BlockId::SCULK_CATALYST
                            })
                    {
                        let candidate = (distance, *position);
                        if prefer_catalyst(nearest, candidate) {
                            nearest = Some(candidate);
                        }
                    }
                }
            }
        }
        let Some((_, catalyst_pos)) = nearest else {
            return false;
        };
        if let Some(charge) = catalyst_experience_charge(experience_eligible, experience_reward) {
            if let Some(entities) = self.block_entities.get(&catalyst_pos.chunk_position())
                && let Some(entity) = entities.get(&catalyst_pos)
                && let Some(catalyst) = entity.as_any().downcast_ref::<SculkCatalystBlockEntity>()
            {
                catalyst
                    .spreader
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .add_cursors(effect_position, charge);
            }
        }
        bloom(self, &catalyst_pos, &effect_position);
        true
    }

    pub async fn unload(self: &Arc<Self>) {
        let mut event =
            crate::plugin::api::events::world::world_load::WorldUnloadEvent::new(self.clone());
        if let Some(server) = self.server.upgrade() {
            server.plugin_manager.fire(&server, &mut event).await;
        }
    }

    pub async fn save(&self) -> Result<(), String> {
        self.save_with_flush(false).await
    }

    pub async fn save_with_flush(&self, flush: bool) -> Result<(), String> {
        for entity in self.entities.load().iter() {
            self.save_entity(entity).await?;
        }
        let chunks: Vec<Vector2<i32>> = self
            .block_entities
            .iter()
            .map(|chunk_block_entities| *chunk_block_entities.key())
            .collect();
        for chunk_pos in chunks {
            self.save_block_entities(chunk_pos);
        }

        if flush {
            let mut portal_poi = self
                .portal_poi
                .try_lock()
                .map_err(|_| "portal POI save lock is unavailable".to_string())?;
            portal_poi
                .save_all()
                .map_err(|error| format!("failed saving portal POI data: {error}"))?;
        } else if let Ok(mut portal_poi) = self.portal_poi.try_lock() {
            let _ = portal_poi.save_all();
        }

        {
            let custom_data = self
                .custom_data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !custom_data.is_empty() {
                let custom_data_path = self
                    .level
                    .level_folder
                    .root_folder
                    .join("pumpkin_custom_data.nbt");
                let nbt = pumpkin_nbt::Nbt::from(custom_data.clone());
                let result = std::fs::write(&custom_data_path, nbt.write());
                if flush {
                    result.map_err(|error| {
                        format!(
                            "failed saving custom world data {}: {error}",
                            custom_data_path.display()
                        )
                    })?;
                }
            }
        }

        if !flush {
            self.level
                .should_save
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.level.level_channel.notify();
        }

        let mut save_event = crate::plugin::api::events::world::world_save::WorldSaveEvent::new(
            format!("{:?}", self.dimension),
        );
        if let Some(server) = self.server.upgrade() {
            server.plugin_manager.fire(&server, &mut save_event).await;
        }
        if flush {
            self.level.flush_entity_data_and_chunks().await?;
        }
        Ok(())
    }

    pub fn set_custom_data(&self, namespace: &str, key: &str, value: pumpkin_nbt::tag::NbtTag) {
        let mut custom_data = self
            .custom_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut namespace_data = custom_data
            .child_tags
            .remove(namespace)
            .and_then(|tag| match tag {
                pumpkin_nbt::tag::NbtTag::Compound(compound) => Some(compound),
                _ => None,
            })
            .unwrap_or_default();

        namespace_data.child_tags.insert(key.into(), value);
        custom_data.child_tags.insert(
            namespace.into(),
            pumpkin_nbt::tag::NbtTag::Compound(namespace_data),
        );
    }

    pub fn get_custom_data(&self, namespace: &str, key: &str) -> Option<pumpkin_nbt::tag::NbtTag> {
        let custom_data = self
            .custom_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        custom_data
            .get(namespace)?
            .extract_compound()?
            .get(key)
            .cloned()
    }

    pub fn remove_custom_data(&self, namespace: &str, key: &str) {
        let mut custom_data = self
            .custom_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let Some(pumpkin_nbt::tag::NbtTag::Compound(mut namespace_data)) =
            custom_data.child_tags.remove(namespace)
        else {
            return;
        };

        namespace_data.child_tags.remove(key);
        if !namespace_data.is_empty() {
            custom_data.child_tags.insert(
                namespace.into(),
                pumpkin_nbt::tag::NbtTag::Compound(namespace_data),
            );
        }
    }

    pub fn has_custom_data(&self, namespace: &str, key: &str) -> bool {
        self.get_custom_data(namespace, key).is_some()
    }

    pub fn populate_chunk(&self, chunk_pos: Vector2<i32>) {
        let mut populate_event =
            crate::plugin::api::events::world::chunk_populate::ChunkPopulateEvent::new(chunk_pos);
        if let Some(server) = self.server.upgrade() {
            server
                .plugin_manager
                .fire_blocking(&server, &mut populate_event);
        }
    }

    pub fn unload_chunk(&self, chunk_pos: Vector2<i32>) {
        let mut unload_event =
            crate::plugin::api::events::world::chunk_unload::ChunkUnloadEvent::new(chunk_pos);
        if let Some(server) = self.server.upgrade() {
            server
                .plugin_manager
                .fire_blocking(&server, &mut unload_event);
        }
    }

    pub fn load_entities(&self, chunk_pos: Vector2<i32>, entity_count: usize) {
        let mut load_event =
            crate::plugin::api::events::world::entities_load::EntitiesLoadEvent::new(
                chunk_pos,
                entity_count,
            );
        if let Some(server) = self.server.upgrade() {
            server
                .plugin_manager
                .fire_blocking(&server, &mut load_event);
        }
    }

    pub fn unload_entities(&self, chunk_pos: Vector2<i32>, entity_count: usize) {
        let mut unload_event =
            crate::plugin::api::events::world::entities_unload::EntitiesUnloadEvent::new(
                chunk_pos,
                entity_count,
            );
        if let Some(server) = self.server.upgrade() {
            server
                .plugin_manager
                .fire_blocking(&server, &mut unload_event);
        }
    }

    pub fn generate_loot(&self, loot_table: String) {
        let mut loot_event =
            crate::plugin::api::events::world::loot_generate::LootGenerateEvent::new(loot_table);
        if let Some(server) = self.server.upgrade() {
            server
                .plugin_manager
                .fire_blocking(&server, &mut loot_event);
        }
    }

    pub fn skip_time(&self, skip_amount: i64) {
        let mut time_event =
            crate::plugin::api::events::world::time_skip::TimeSkipEvent::new(skip_amount);
        if let Some(server) = self.server.upgrade() {
            server
                .plugin_manager
                .fire_blocking(&server, &mut time_event);
        }
    }

    pub fn trigger_raid(&self, pos: BlockPos) {
        let mut raid_event =
            crate::plugin::api::events::raid::raid_trigger::RaidTriggerEvent::new(pos);
        if let Some(server) = self.server.upgrade() {
            server
                .plugin_manager
                .fire_blocking(&server, &mut raid_event);
        }
    }

    pub fn spawn_raid_wave(&self, wave: u32, pos: BlockPos) {
        let mut wave_event =
            crate::plugin::api::events::raid::raid_spawn_wave::RaidSpawnWaveEvent::new(wave, pos);
        if let Some(server) = self.server.upgrade() {
            server
                .plugin_manager
                .fire_blocking(&server, &mut wave_event);
        }
    }

    pub fn finish_raid(&self, victory: bool) {
        let mut raid_event =
            crate::plugin::api::events::raid::raid_finish::RaidFinishEvent::new(victory);
        if let Some(server) = self.server.upgrade() {
            server
                .plugin_manager
                .fire_blocking(&server, &mut raid_event);
        }
    }

    pub fn stop_raid(&self, reason: String) {
        let mut raid_event =
            crate::plugin::api::events::raid::raid_stop::RaidStopEvent::new(reason);
        if let Some(server) = self.server.upgrade() {
            server
                .plugin_manager
                .fire_blocking(&server, &mut raid_event);
        }
    }

    pub fn async_structure_generate(
        &self,
        world_name: String,
        structure_name: String,
        pos: BlockPos,
    ) {
        let mut event = crate::plugin::api::events::world::async_structure_generate::AsyncStructureGenerateEvent::new(
            world_name,
            structure_name,
            pos,
        );
        if let Some(server) = self.server.upgrade() {
            server.plugin_manager.fire_blocking(&server, &mut event);
        }
    }

    pub fn async_structure_spawn(&self, world_name: String, structure_name: String, pos: BlockPos) {
        let mut event =
            crate::plugin::api::events::world::async_structure_spawn::AsyncStructureSpawnEvent::new(
                world_name,
                structure_name,
                pos,
            );
        if let Some(server) = self.server.upgrade() {
            server.plugin_manager.fire_blocking(&server, &mut event);
        }
    }
}

fn bedrock_block_breaking_rate(speed: f32) -> i32 {
    (speed.clamp(0.0, 1.0) * f32::from(u16::MAX)) as i32
}

pub struct WorldPortal(pub Arc<World>);

// Pure Beauty :cap:
impl WorldPortalExt for WorldPortal {
    fn can_place_at(
        &self,
        block: &pumpkin_data::Block,
        state: &BlockState,
        block_accessor: &dyn BlockAccessor,
        block_pos: &BlockPos,
    ) -> bool {
        self.0.block_registry.can_place_at(
            None,
            Some(&self.0),
            block_accessor,
            None,
            block,
            state,
            block_pos,
            None,
            None,
        )
    }

    fn mirror(&self, block: &Block, state_id: BlockStateId, mirror: Mirror) -> &'static BlockState {
        self.0.block_registry.mirror(block, state_id, mirror)
    }

    fn rotate(
        &self,
        block: &Block,
        state_id: BlockStateId,
        rotation: Rotation,
    ) -> &'static BlockState {
        self.0.block_registry.rotate(block, state_id, rotation)
    }

    fn spawn_mobs_for_chunk_generation(
        &self,
        cache: &mut dyn GenerationCache,
        biome: &'static Biome,
        chunk_x: i32,
        chunk_z: i32,
    ) {
        natural_spawner::spawn_mobs_for_chunk_generation(&self.0, cache, biome, chunk_x, chunk_z);
    }

    fn spawn_structure_entities(&self, entities: Vec<NbtCompound>) {
        for nbt in entities {
            let Some(id) = nbt.get_string("id") else {
                continue;
            };
            let Some(entity_type) =
                EntityType::from_name(id.strip_prefix("minecraft:").unwrap_or(id))
            else {
                warn!("Unknown structure entity type: {id}");
                continue;
            };
            let entity = from_type(
                entity_type,
                Vector3::new(0.0, 0.0, 0.0),
                &self.0,
                Uuid::new_v4(),
            );
            entity.get_entity().read_nbt_non_mut(&nbt);
            entity.read_nbt_non_mut(&nbt);
            self.0.spawn_entity(entity);
        }
    }
}

struct CubicCurve {
    a: f32,
    b: f32,
    c: f32,
}

impl CubicCurve {
    fn new(v1: f32, v2: f32) -> Self {
        Self {
            a: 3.0 * v1 - 3.0 * v2 + 1.0,
            b: -6.0 * v1 + 3.0 * v2,
            c: 3.0 * v1,
        }
    }

    fn sample(&self, t: f32) -> f32 {
        ((self.a * t + self.b) * t + self.c) * t
    }

    fn sample_gradient(&self, t: f32) -> f32 {
        (3.0 * self.a * t + 2.0 * self.b) * t + self.c
    }
}

/// Calculates the celestial (sun) angle fraction in `[0.0, 1.0]`.
/// Matches vanilla 26.2 `EnvironmentAttributes.SUN_ANGLE` easing with `symmetricCubicBezier(0.362, 0.241)`.
#[must_use]
pub fn calculate_celestial_angle(time_of_day: i64) -> f32 {
    let ticks = time_of_day.rem_euclid(24000);
    let alpha = if ticks < 6000 {
        (ticks + 18000) as f32 / 24000.0
    } else {
        (ticks - 6000) as f32 / 24000.0
    };

    let x_curve = CubicCurve::new(0.362, 0.638);
    let y_curve = CubicCurve::new(0.241, 0.759);

    let mut t = alpha;
    let mut solved = false;
    for _ in 0..4 {
        let error = x_curve.sample(t) - alpha;
        if error.abs() < 1e-5 {
            solved = true;
            break;
        }
        let gradient = x_curve.sample_gradient(t);
        if gradient < 1e-5 {
            break;
        }
        t -= (error / gradient).clamp(-0.25, 0.25);
    }

    if !solved {
        let mut t0 = 0.0f32;
        let mut t1 = 1.0f32;
        for _ in 0..64 {
            if t0 >= t1 {
                break;
            }
            let error = x_curve.sample(t) - alpha;
            if error.abs() < 1e-5 {
                break;
            }
            if error < 0.0 {
                t0 = t;
            } else {
                t1 = t;
            }
            t = f32::midpoint(t1, t0);
        }
    }

    y_curve.sample(t)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Weak};

    use arc_swap::ArcSwap;
    use pumpkin_config::world::LevelConfig;
    use pumpkin_data::{
        Block,
        block_properties::{ChestLikeProperties, ChestType, HorizontalFacing, WaterLikeProperties},
        dimension::Dimension,
        entity::EntityType,
        fluid::Fluid,
    };
    use pumpkin_nbt::compound::NbtCompound;
    use pumpkin_util::math::{position::BlockPos, vector2::Vector2, vector3::Vector3};
    use pumpkin_util::world_seed::Seed;
    use pumpkin_world::world_info::LevelData;
    use pumpkin_world::{
        chunk::{ChunkData, format::anvil::SingleChunkDataSerializer},
        level::Level,
    };
    use tempfile::TempDir;
    use tokio::time::{Duration, timeout};
    use uuid::Uuid;

    use super::{
        World, bedrock_block_breaking_rate, bedrock_chest_block_actor, catalyst_experience_charge,
        prefer_catalyst, sculk_event_listeners, shrieker_source_eligible,
        traverse_vibration_blocks, within_sculk_listener_radius,
    };
    use crate::item::items::debug_stick::DEBUG_STICK_BLOCK_UPDATE_FLAGS;
    use pumpkin_world::world::BlockFlags;

    #[test]
    fn catalyst_selects_one_nearest_candidate_with_stable_ties() {
        let near = (2.0, BlockPos::new(1, 2, 3));
        let far = (4.0, BlockPos::new(0, 0, 0));
        assert!(prefer_catalyst(None, far));
        assert!(prefer_catalyst(Some(far), near));
        assert!(!prefer_catalyst(Some(near), far));
        assert!(prefer_catalyst(Some((2.0, BlockPos::new(2, 2, 3))), near));
    }

    #[test]
    fn catalyst_charges_only_positive_eligible_experience() {
        assert_eq!(catalyst_experience_charge(true, 7), Some(7));
        assert_eq!(catalyst_experience_charge(true, 0), None);
        assert_eq!(catalyst_experience_charge(false, 7), None);
    }

    #[test]
    fn shrieker_keeps_block_radius_eligibility_when_exact_distance_exceeds_eight() {
        let event_position = Vector3::new(8.99, 0.5, 0.5);
        let event_block = BlockPos::floored(event_position.x, event_position.y, event_position.z);
        let listener_block = BlockPos::new(0, 0, 0);
        let listener_center = listener_block.to_centered_f64();
        let exact_distance = (listener_center.x - event_position.x)
            .hypot(listener_center.y - event_position.y)
            .hypot(listener_center.z - event_position.z);

        assert!(within_sculk_listener_radius(event_block, listener_block, 8));
        assert!(exact_distance > 8.0);
    }

    #[test]
    fn shrieker_accepts_player_owned_item_entity() {
        assert!(shrieker_source_eligible(false, false, true, false));
    }

    #[test]
    fn shrieker_rejects_ownerless_item_entity() {
        assert!(!shrieker_source_eligible(false, false, false, false));
    }

    #[test]
    fn sculk_vibration_and_shrieker_routes_use_their_generated_tags() {
        use pumpkin_data::game_event::GameEvent;

        assert_eq!(sculk_event_listeners(GameEvent::Step), (true, false));
        assert_eq!(
            sculk_event_listeners(GameEvent::SculkSensorTendrilsClicking),
            (true, true)
        );
        assert_eq!(
            sculk_event_listeners(GameEvent::JukeboxPlay),
            (false, false)
        );
    }

    fn test_world(root: &Path) -> Arc<World> {
        let config = LevelConfig {
            autosave_ticks: 0,
            ..LevelConfig::default()
        };
        let level = Level::from_root_folder(&config, root.to_path_buf(), 0, Dimension::OVERWORLD);
        Arc::new(World::load(
            level,
            Arc::new(ArcSwap::new(Arc::new(LevelData::default(Seed(0))))),
            Dimension::OVERWORLD,
            crate::block::registry::default_registry(),
            Weak::new(),
        ))
    }

    async fn debug_stick_flag_fixture(flags: BlockFlags) -> (pumpkin_data::BlockStateId, bool) {
        let temp_dir = TempDir::new().expect("debug-stick flag fixture tempdir");
        let world = test_world(temp_dir.path());
        world
            .level
            .loaded_chunks
            .insert(Vector2::new(0, 0), ChunkData::empty_sync(0, 0));

        let target = BlockPos::new(0, 64, 0);
        let neighbor = BlockPos::new(1, 64, 0);
        let gate = &Block::OAK_FENCE_GATE;
        let wire = &Block::REDSTONE_WIRE;
        let mut neighbor_props = wire
            .properties(wire.default_state.id)
            .expect("redstone wire properties")
            .to_props();
        for (name, value) in &mut neighbor_props {
            if *name == "west" {
                *value = "side";
            }
        }
        let neighbor_initial = wire.from_properties(&neighbor_props).to_state_id(wire);
        let mut open_props = gate
            .properties(gate.default_state.id)
            .expect("fence gate properties")
            .to_props();
        for (name, value) in &mut open_props {
            if *name == "open" {
                *value = "true";
            }
        }
        let target_open = gate.from_properties(&open_props).to_state_id(gate);

        // Seed a closed gate and an adjacent connected wire without invoking
        // their placement/shape callbacks.
        let setup_flags = BlockFlags::NOTIFY_LISTENERS | BlockFlags::UPDATE_KNOWN_SHAPE;
        world.set_block_state(
            &BlockPos::new(0, 63, 0),
            Block::STONE.default_state.id,
            setup_flags,
        );
        world.set_block_state(
            &BlockPos::new(1, 63, 0),
            Block::STONE.default_state.id,
            setup_flags,
        );
        world.set_block_state(&target, gate.default_state.id, setup_flags);
        world.set_block_state(&neighbor, neighbor_initial, setup_flags);
        world
            .unsent_block_changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();

        // Opening the gate is the Debug Stick mutation. Vanilla flags 18 keep
        // the adjacent wire's stale shape and still notify clients.
        assert_eq!(world.get_block_state_id(&neighbor), neighbor_initial);
        world.set_block_state(&target, target_open, flags);
        let neighbor_after = world.get_block_state_id(&neighbor);
        let client_update = world
            .unsent_block_changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&target)
            .copied()
            == Some(target_open);
        (neighbor_after, client_update)
    }

    #[tokio::test]
    async fn debug_stick_flags_skip_neighbor_shape_update_but_notify_client() {
        let expected_stale_neighbor = {
            let wire = &Block::REDSTONE_WIRE;
            let mut props = wire
                .properties(wire.default_state.id)
                .expect("redstone wire properties")
                .to_props();
            for (name, value) in &mut props {
                if *name == "west" {
                    *value = "side";
                }
            }
            wire.from_properties(&props).to_state_id(wire)
        };
        let (neighbor_after, client_update) =
            debug_stick_flag_fixture(DEBUG_STICK_BLOCK_UPDATE_FLAGS).await;
        assert_eq!(
            neighbor_after, expected_stale_neighbor,
            "Debug Stick flags must suppress the adjacent wire shape callback"
        );
        assert!(
            client_update,
            "Debug Stick flags must retain client notification"
        );
    }

    fn set_persistence_marker(entity: &Arc<dyn crate::entity::EntityBase>, marker: &str) {
        let mut custom_data = entity
            .get_entity()
            .custom_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        custom_data.put_string("marker", marker.to_owned());
    }

    async fn reopened_entity_chunk(world: &Arc<World>, pos: Vector2<i32>) -> Vec<NbtCompound> {
        let mut receiver = world.level.receive_entity_chunks(vec![pos]);
        let (chunk, _) = timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("reopened entity chunk notification timed out")
            .expect("reopened entity chunk notification missing");
        let chunk = chunk.upgrade().expect("reopened entity chunk dropped");
        chunk
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn world_entity_persistence_e2e_spawn_move_stop_reopen() {
        let temp_dir = TempDir::new().expect("entity persistence tempdir");
        let world = test_world(temp_dir.path());
        let moved_uuid = Uuid::from_u128(0x262);
        let new_uuid = Uuid::from_u128(0x2622);
        let source_chunk = Vector2::new(0, 0);
        let moved_chunk = Vector2::new(2, 0);
        let new_spawn_chunk = Vector2::new(5, 0);

        // Use the normal concrete entity factory and live world index. The first
        // save deliberately leaves an old cached snapshot so a boundary move
        // must remove it rather than resurrecting a duplicate on reopen.
        let moved = crate::entity::r#type::from_type(
            &EntityType::ARMOR_STAND,
            Vector3::new(15.5, 64.0, 0.5),
            &world,
            moved_uuid,
        );
        world.spawn_entity_non_save(moved.clone());
        set_persistence_marker(&moved, "spawned");
        world.save().await.expect("initial live entity save");

        moved.get_entity().set_pos(Vector3::new(32.5, 65.0, 0.5));
        set_persistence_marker(&moved, "moved-live");

        // This entity is a new spawn with no earlier entity-file snapshot and
        // is saved directly to a non-resident destination chunk.
        let new_spawn = crate::entity::r#type::from_type(
            &EntityType::ARMOR_STAND,
            Vector3::new(80.5, 70.0, 0.5),
            &world,
            new_uuid,
        );
        world.spawn_entity_non_save(new_spawn.clone());
        set_persistence_marker(&new_spawn, "new-spawn-live");

        world.save().await.expect("boundary and new-spawn save");
        world.shutdown().await.expect("world stop persistence");

        // Reopen through a separate World/Level instance and inspect the
        // actual entity storage path, not a low-level synthetic fixture.
        let reopened = test_world(temp_dir.path());
        let old_entities = reopened_entity_chunk(&reopened, source_chunk).await;
        let moved_entities = reopened_entity_chunk(&reopened, moved_chunk).await;
        let new_entities = reopened_entity_chunk(&reopened, new_spawn_chunk).await;

        assert!(old_entities.is_empty(), "old chunk retained a moved entity");
        assert_eq!(moved_entities.len(), 1, "moved chunk entity count");
        assert_eq!(new_entities.len(), 1, "new-spawn chunk entity count");

        let moved_nbt = &moved_entities[0];
        assert_eq!(moved_nbt.get_uuid("UUID"), Some(moved_uuid));
        assert_eq!(moved_nbt.get_string("id"), Some("minecraft:armor_stand"));
        let moved_pos = moved_nbt.get_list("Pos").expect("moved Pos");
        assert_eq!(moved_pos[0].extract_double(), Some(32.5));
        assert_eq!(moved_pos[1].extract_double(), Some(65.0));
        assert_eq!(moved_pos[2].extract_double(), Some(0.5));
        assert_eq!(
            moved_nbt
                .get_compound("PumpkinCustomData")
                .and_then(|data| data.get_string("marker")),
            Some("moved-live")
        );

        let new_nbt = &new_entities[0];
        assert_eq!(new_nbt.get_uuid("UUID"), Some(new_uuid));
        assert_eq!(new_nbt.get_string("id"), Some("minecraft:armor_stand"));
        let new_pos = new_nbt.get_list("Pos").expect("new-spawn Pos");
        assert_eq!(new_pos[0].extract_double(), Some(80.5));
        assert_eq!(new_pos[1].extract_double(), Some(70.0));
        assert_eq!(new_pos[2].extract_double(), Some(0.5));
        assert_eq!(
            new_nbt
                .get_compound("PumpkinCustomData")
                .and_then(|data| data.get_string("marker")),
            Some("new-spawn-live")
        );

        let all_entities = moved_entities
            .iter()
            .chain(new_entities.iter())
            .collect::<Vec<_>>();
        let uuids = all_entities
            .iter()
            .map(|entity| entity.get_uuid("UUID").expect("entity UUID"))
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(uuids.len(), all_entities.len(), "reopened UUID duplicate");

        reopened.shutdown().await.expect("reopened world shutdown");
    }

    #[tokio::test]
    async fn parser_only_map_entity_does_not_match_furnace_state() {
        let temp_dir = TempDir::new().expect("map mismatch tempdir");
        let world = test_world(temp_dir.path());
        let position = BlockPos::new(1, 64, 1);
        let chunk_pos = position.chunk_position();

        world.level.loaded_chunks.insert(
            chunk_pos,
            pumpkin_world::chunk::ChunkData::empty_sync(chunk_pos.x, chunk_pos.y),
        );
        world.set_block_state(
            &position,
            Block::FURNACE.default_state.id,
            super::BlockFlags::FORCE_STATE,
        );
        world.remove_block_entity(&position);

        let mut map_nbt = NbtCompound::new();
        map_nbt.put_string("id", "minecraft:map".to_owned());
        map_nbt.put_int("x", position.0.x);
        map_nbt.put_int("y", position.0.y);
        map_nbt.put_int("z", position.0.z);
        let live_map =
            crate::block::entities::block_entity_from_nbt(&map_nbt).expect("map parser fixture");
        world
            .block_entities
            .entry(chunk_pos)
            .or_default()
            .insert(position, live_map);
        assert!(world.get_block_entity(&position).is_none());
        world.save_block_entities(chunk_pos);
        assert!(
            world
                .level
                .read_chunk_sync(&chunk_pos, |chunk| {
                    chunk
                        .pending_block_entities
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains_key(&position)
                })
                .unwrap_or(false)
                == false
        );
        world.block_entities.remove(&chunk_pos);
        world.add_block_entity_nbt(position, &map_nbt);
        world.migrate_pending_block_entities(chunk_pos);

        assert!(world.get_block_entity(&position).is_none());
        assert!(
            world
                .level
                .read_chunk_sync(&chunk_pos, |chunk| {
                    chunk
                        .pending_block_entities
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains_key(&position)
                })
                .unwrap_or(false)
        );

        world.shutdown().await.expect("map mismatch shutdown");
    }

    #[tokio::test]
    async fn block_entity_replacement_removes_pending_nbt_before_new_block() {
        let temp_dir = TempDir::new().expect("block entity persistence tempdir");
        let world = test_world(temp_dir.path());
        let position = BlockPos::new(1, 64, 1);
        let chunk_pos = position.chunk_position();

        world.level.loaded_chunks.insert(
            chunk_pos,
            pumpkin_world::chunk::ChunkData::empty_sync(chunk_pos.x, chunk_pos.y),
        );
        world.set_block_state(
            &position,
            Block::CHEST.default_state.id,
            super::BlockFlags::FORCE_STATE,
        );

        let mut chest_nbt = NbtCompound::new();
        chest_nbt.put_string("id", "minecraft:chest".to_owned());
        chest_nbt.put_int("x", position.0.x);
        chest_nbt.put_int("y", position.0.y);
        chest_nbt.put_int("z", position.0.z);
        world.add_block_entity_nbt(position, &chest_nbt);

        world.set_block_state(
            &position,
            Block::AIR.default_state.id,
            super::BlockFlags::FORCE_STATE,
        );

        let pending_after_removal = world.level.read_chunk_sync(&chunk_pos, |chunk| {
            chunk
                .pending_block_entities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&position)
                .cloned()
        });
        world.set_block_state(
            &position,
            Block::FURNACE.default_state.id,
            super::BlockFlags::FORCE_STATE,
        );
        assert_eq!(
            world
                .get_block_entity(&position)
                .expect("furnace block entity")
                .resource_location(),
            "minecraft:furnace"
        );

        let serialized = world
            .level
            .read_chunk_sync(&chunk_pos, |chunk| {
                chunk.to_bytes().expect("serialize chunk")
            })
            .expect("loaded chunk");
        let reloaded = <pumpkin_world::chunk::ChunkData as SingleChunkDataSerializer>::from_bytes(
            &serialized,
            chunk_pos,
        )
        .expect("reload chunk");
        let pending_after_reload = reloaded
            .pending_block_entities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&position)
            .and_then(|nbt| nbt.get_string("id"))
            .map(str::to_owned);

        world
            .shutdown()
            .await
            .expect("shutdown block entity replacement");
        assert!(
            pending_after_removal.flatten().is_none(),
            "removed block entity left stale pending NBT"
        );
        assert_eq!(pending_after_reload.as_deref(), Some("minecraft:furnace"));
    }

    #[tokio::test]
    async fn mismatched_block_entity_stays_pending_without_promotion() {
        let temp_dir = TempDir::new().expect("block entity mismatch tempdir");
        let world = test_world(temp_dir.path());
        let position = BlockPos::new(1, 64, 1);
        let chunk_pos = position.chunk_position();

        world.level.loaded_chunks.insert(
            chunk_pos,
            pumpkin_world::chunk::ChunkData::empty_sync(chunk_pos.x, chunk_pos.y),
        );
        world.set_block_state(
            &position,
            Block::STONE.default_state.id,
            super::BlockFlags::FORCE_STATE,
        );

        let mut chest_nbt = NbtCompound::new();
        chest_nbt.put_string("id", "minecraft:chest".to_owned());
        chest_nbt.put_int("x", position.0.x);
        chest_nbt.put_int("y", position.0.y);
        chest_nbt.put_int("z", position.0.z);
        chest_nbt.put_string("FutureField", "must-remain-pending".to_owned());
        world.add_block_entity_nbt(position, &chest_nbt);

        world.migrate_pending_block_entities(chunk_pos);

        assert!(
            world.get_block_entity(&position).is_none(),
            "known block entity IDs must not promote on a block-state mismatch"
        );
        let pending = world
            .level
            .read_chunk_sync(&chunk_pos, |chunk| {
                chunk
                    .pending_block_entities
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&position)
                    .cloned()
            })
            .flatten()
            .expect("mismatched entity remains pending");
        assert_eq!(
            pending.get_string("FutureField"),
            Some("must-remain-pending")
        );

        let serialized = world
            .level
            .read_chunk_sync(&chunk_pos, |chunk| {
                chunk.to_bytes().expect("serialize chunk")
            })
            .expect("loaded chunk");
        let reloaded = <pumpkin_world::chunk::ChunkData as SingleChunkDataSerializer>::from_bytes(
            &serialized,
            chunk_pos,
        )
        .expect("reload chunk");
        let reloaded = reloaded
            .pending_block_entities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            reloaded
                .get(&position)
                .and_then(|nbt| nbt.get_string("FutureField")),
            Some("must-remain-pending")
        );

        world
            .shutdown()
            .await
            .expect("shutdown mismatch test world");
    }

    #[test]
    fn liquid_block_states_preserve_source_flow_and_falling_depths() {
        // Liquid block level and fluid amount are different: all falling levels
        // resolve to amount 8, rather than wrapping through the fluid state array.
        let amounts = [8, 7, 6, 5, 4, 3, 2, 1, 8, 8, 8, 8, 8, 8, 8, 8];
        for (block, expected_fluid) in [
            (&Block::WATER, &Fluid::FLOWING_WATER),
            (&Block::LAVA, &Fluid::FLOWING_LAVA),
        ] {
            for (level, amount) in amounts.into_iter().enumerate() {
                let id = WaterLikeProperties { level: level as u8 }.to_state_id(block);
                let (fluid, state) = World::fluid_state_from_block_state(id);
                assert_eq!(fluid.id, expected_fluid.id);
                assert_eq!(state.level, amount);
                assert!((state.height - f32::from(amount) / 9.0).abs() < f32::EPSILON);
                assert_eq!(state.is_source, level == 0);
                assert_eq!(state.is_still, level == 0);
                assert_eq!(state.falling, level >= 8);
                assert!(!state.is_empty);
                assert_eq!(
                    state.block_state_id,
                    WaterLikeProperties {
                        level: level.min(8) as u8
                    }
                    .to_state_id(block)
                );
            }
        }
    }

    #[test]
    fn aquatic_and_waterlogged_blocks_contain_source_water() {
        let wet_stairs = Block::OAK_STAIRS
            .set_waterlogged(Block::OAK_STAIRS.default_state.id, true)
            .unwrap();
        for id in [
            Block::KELP.default_state.id,
            Block::KELP_PLANT.default_state.id,
            Block::SEAGRASS.default_state.id,
            Block::TALL_SEAGRASS.default_state.id,
            Block::BUBBLE_COLUMN.default_state.id,
            wet_stairs,
        ] {
            let (fluid, state) = World::fluid_state_from_block_state(id);
            assert!(fluid.matches_type(&Fluid::WATER));
            assert!(state.is_source && state.is_still && !state.is_empty && !state.falling);
            assert_eq!(state.level, 8);
            assert!((state.height - 8.0 / 9.0).abs() < f32::EPSILON);
        }

        for block in [&Block::AIR, &Block::OAK_STAIRS, &Block::WATER_CAULDRON] {
            let (fluid, state) = World::fluid_state_from_block_state(block.default_state.id);
            assert_eq!(fluid.id, Fluid::EMPTY.id);
            assert!(state.is_empty);
            assert_eq!(state.height, 0.0);
        }
    }

    #[test]
    fn a_shapeless_block_never_stops_a_ray() {
        // The ray always starts inside a block, usually air, and that block must not
        // count as a hit or every raycast would stop where it began.
        let pos = BlockPos::new(10, 64, 10);
        let from = pumpkin_util::math::vector3::Vector3::new(10.5, 64.5, 10.5);
        let to = pumpkin_util::math::vector3::Vector3::new(20.5, 64.5, 10.5);

        assert!(
            super::World::clip_outline_shapes(Block::AIR.default_state, &pos, from, to).is_none()
        );
        assert!(
            super::World::clip_outline_shapes(Block::STONE.default_state, &pos, from, to).is_some()
        );
    }

    #[test]
    fn bedrock_block_breaking_rate_uses_progress_per_tick() {
        assert_eq!(bedrock_block_breaking_rate(0.0), 0);
        assert_eq!(bedrock_block_breaking_rate(1.0 / 30.0), 2_184);
        assert_eq!(bedrock_block_breaking_rate(1.0), 65_535);
    }

    #[test]
    fn bedrock_double_chest_block_actor_identifies_pair_and_lead() {
        let position = BlockPos::new(5, 64, 7);
        let properties = ChestLikeProperties {
            facing: HorizontalFacing::North,
            r#type: ChestType::Right,
            waterlogged: false,
        };
        let actor =
            bedrock_chest_block_actor(properties.to_state_id(&Block::CHEST), position).unwrap();

        assert_eq!(actor.get_int("pairx"), Some(4));
        assert_eq!(actor.get_int("pairz"), Some(7));
        assert_eq!(actor.get_bool("pairlead"), Some(true));
    }

    #[test]
    fn vibration_traversal_checks_nonzero_same_cell_trace() {
        let mut visited = Vec::new();
        assert!(!traverse_vibration_blocks(
            Vector3::new(0.1, 0.2, 0.3),
            Vector3::new(0.8, 0.7, 0.6),
            |block| {
                visited.push(block);
                false
            }
        ));
        assert_eq!(visited, [BlockPos::new(0, 0, 0)]);
        assert!(traverse_vibration_blocks(
            Vector3::new(0.1, 0.2, 0.3),
            Vector3::new(0.8, 0.7, 0.6),
            |block| block == BlockPos::new(0, 0, 0)
        ));
    }

    #[test]
    fn vibration_traversal_checks_start_and_endpoint_cells() {
        let from = Vector3::new(0.5, 0.5, 0.5);
        let to = Vector3::new(2.5, 0.5, 0.5);
        assert!(traverse_vibration_blocks(from, to, |block| block == BlockPos::new(0, 0, 0)));
        assert!(traverse_vibration_blocks(from, to, |block| block == BlockPos::new(2, 0, 0)));
    }

    #[test]
    fn vibration_traversal_ties_step_z_then_y_then_x() {
        let mut edge = Vec::new();
        traverse_vibration_blocks(
            Vector3::new(0.5, 0.5, 0.5),
            Vector3::new(2.5, 2.5, 0.5),
            |block| {
                edge.push(block);
                false
            },
        );
        assert_eq!(
            edge,
            [
                BlockPos::new(0, 0, 0),
                BlockPos::new(0, 1, 0),
                BlockPos::new(1, 1, 0),
                BlockPos::new(1, 2, 0),
                BlockPos::new(2, 2, 0),
            ]
        );

        let mut corner = Vec::new();
        traverse_vibration_blocks(
            Vector3::new(0.5, 0.5, 0.5),
            Vector3::new(1.5, 1.5, 1.5),
            |block| {
                corner.push(block);
                false
            },
        );
        assert_eq!(
            corner,
            [
                BlockPos::new(0, 0, 0),
                BlockPos::new(0, 0, 1),
                BlockPos::new(0, 1, 1),
                BlockPos::new(1, 1, 1),
            ]
        );
    }

    #[test]
    fn vibration_traversal_handles_negative_coordinates() {
        let mut visited = Vec::new();
        traverse_vibration_blocks(
            Vector3::new(-0.5, 0.5, 0.5),
            Vector3::new(-2.5, 0.5, 0.5),
            |block| {
                visited.push(block);
                false
            },
        );
        assert_eq!(
            visited,
            [
                BlockPos::new(-1, 0, 0),
                BlockPos::new(-2, 0, 0),
                BlockPos::new(-3, 0, 0),
            ]
        );
    }

    #[test]
    fn vibration_traversal_zero_length_is_a_miss() {
        let point = Vector3::new(0.5, 0.5, 0.5);
        let mut visited = false;
        assert!(!traverse_vibration_blocks(point, point, |_| {
            visited = true;
            true
        }));
        assert!(!visited);
    }

    #[test]
    fn game_rules_registry() {
        use pumpkin_data::game_rules::{GameRule, GameRuleRegistry, GameRuleValue};

        let mut registry = GameRuleRegistry::default();
        match registry.get(&GameRule::KeepInventory) {
            GameRuleValue::Bool(v) => assert!(!v),
            GameRuleValue::Int(_) => panic!("expected bool"),
        }

        match registry.get_mut(&GameRule::KeepInventory) {
            GameRuleValue::Bool(v) => *v = true,
            GameRuleValue::Int(_) => panic!("expected bool"),
        }

        match registry.get(&GameRule::KeepInventory) {
            GameRuleValue::Bool(v) => assert!(v),
            GameRuleValue::Int(_) => panic!("expected bool"),
        }

        match registry.get(&GameRule::RandomTickSpeed) {
            GameRuleValue::Int(v) => assert_eq!(*v, 3),
            GameRuleValue::Bool(_) => panic!("expected int"),
        }

        match registry.get_mut(&GameRule::RandomTickSpeed) {
            GameRuleValue::Int(v) => *v = 20,
            GameRuleValue::Bool(_) => panic!("expected int"),
        }

        match registry.get(&GameRule::RandomTickSpeed) {
            GameRuleValue::Int(v) => assert_eq!(*v, 20),
            GameRuleValue::Bool(_) => panic!("expected int"),
        }
    }
}
