use std::sync::Arc;

use crate::block::entities::sculk_shrieker::SculkShriekerBlockEntity;
use crate::block::{BlockBehaviour, BlockMetadata, BrokenArgs, OnPlaceArgs, OnScheduledTickArgs};
use crate::entity::{EntityBase, r#type::from_type};
use crate::world::World;
use pumpkin_data::potion::Effect;
use pumpkin_data::{
    BlockId, BlockStateId,
    block_properties::SculkShriekerLikeProperties,
    effect::StatusEffect,
    entity::EntityType,
    game_event::GameEvent,
    sound::{Sound, SoundCategory},
    world::WorldEvent,
};
use pumpkin_util::{Difficulty, math::position::BlockPos};
use pumpkin_world::tick::TickPriority;
use pumpkin_world::world::BlockFlags;
use rand::RngExt;

const SHRIEK_TICKS: u8 = 90;
const DARKNESS_RADIUS: f64 = 40.0;
const DARKNESS_DURATION: i32 = 260;
const WARN_COOLDOWN: i32 = 200;
const WARN_DECAY: i32 = 12_000;
const WARN_CAP: i32 = 4;
const TRACKER_KEY: &str = "warden_spawn_tracker";
const WARDEN_SPAWN_ATTEMPTS: usize = 20;
const WARDEN_SPAWN_HORIZONTAL_RANGE: i32 = 5;
const WARDEN_SPAWN_VERTICAL_RANGE: i32 = 6;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct WardenSpawnTracker {
    ticks_since_last_warning: i32,
    warning_level: i32,
    cooldown_ticks: i32,
}

impl WardenSpawnTracker {
    fn read(player: &crate::entity::player::Player) -> Self {
        let entity = player.get_entity();
        let data = entity
            .custom_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(pumpkin_nbt::tag::NbtTag::Compound(nbt)) = data.get(TRACKER_KEY) else {
            return Self::default();
        };
        Self::from_nbt(nbt)
    }

    fn from_nbt(nbt: &pumpkin_nbt::compound::NbtCompound) -> Self {
        Self {
            ticks_since_last_warning: nbt.get_int("ticks_since_last_warning").unwrap_or(0).max(0),
            warning_level: nbt.get_int("warning_level").unwrap_or(0).clamp(0, WARN_CAP),
            cooldown_ticks: nbt.get_int("cooldown_ticks").unwrap_or(0).max(0),
        }
    }

    fn to_nbt(self) -> pumpkin_nbt::compound::NbtCompound {
        let mut nbt = pumpkin_nbt::compound::NbtCompound::new();
        nbt.put_int(
            "ticks_since_last_warning",
            self.ticks_since_last_warning.max(0),
        );
        nbt.put_int("warning_level", self.warning_level.clamp(0, WARN_CAP));
        nbt.put_int("cooldown_ticks", self.cooldown_ticks.max(0));
        nbt
    }

    fn write(self, player: &crate::entity::player::Player) {
        let entity = player.get_entity();
        entity
            .custom_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .put_compound(TRACKER_KEY, self.to_nbt());
    }

    fn tick(&mut self) {
        if self.ticks_since_last_warning >= WARN_DECAY {
            self.warning_level = self.warning_level.saturating_sub(1);
            self.ticks_since_last_warning = 0;
        } else {
            self.ticks_since_last_warning = self.ticks_since_last_warning.saturating_add(1);
        }
        self.cooldown_ticks = self.cooldown_ticks.saturating_sub(1);
    }

    fn warn(&mut self) -> bool {
        if self.cooldown_ticks > 0 {
            return false;
        }
        self.ticks_since_last_warning = 0;
        self.cooldown_ticks = WARN_COOLDOWN;
        self.warning_level = (self.warning_level + 1).min(WARN_CAP);
        true
    }
}

pub(crate) fn tick_warden_spawn_tracker(player: &crate::entity::player::Player) {
    let mut tracker = WardenSpawnTracker::read(player);
    tracker.tick();
    tracker.write(player);
}

pub struct SculkShriekerBlock;

impl BlockMetadata for SculkShriekerBlock {
    fn ids() -> Box<[BlockId]> {
        [BlockId::SCULK_SHRIEKER].into()
    }
}

impl SculkShriekerBlock {
    fn player_source(
        world: &World,
        source: uuid::Uuid,
    ) -> Option<Arc<crate::entity::player::Player>> {
        let entity = world.get_entity_by_uuid(source)?;
        if let Some(player) = entity.get_player() {
            return world.get_player_by_uuid(player.gameprofile.id);
        }
        if let Some(owner) = entity
            .get_item_entity()
            .and_then(crate::entity::item::ItemEntity::get_owner)
            .and_then(|owner| world.get_player_by_uuid(owner))
        {
            return Some(owner);
        }
        if let Some(owner_id) = entity.get_owner_id()
            && let Some(owner) = world.get_entity_by_id(owner_id)
            && let Some(player) = owner.get_player()
        {
            return world.get_player_by_uuid(player.gameprofile.id);
        }
        entity
            .get_entity()
            .passengers
            .lock()
            .ok()?
            .iter()
            .find_map(|passenger| {
                passenger
                    .get_player()
                    .and_then(|player| world.get_player_by_uuid(player.gameprofile.id))
            })
    }

    pub fn try_activate(world: &Arc<World>, pos: &BlockPos, source: Option<uuid::Uuid>) -> bool {
        let block = world.get_block(pos);
        if block.id != BlockId::SCULK_SHRIEKER {
            return false;
        }
        let state = world.get_block_state(pos);
        let mut props = SculkShriekerLikeProperties::from_state_id(state.id);
        if props.shrieking {
            return false;
        }
        props.shrieking = true;
        world.set_block_state(pos, props.to_state_id(block), BlockFlags::NOTIFY_ALL);
        world.play_sound(
            Sound::BlockSculkShriekerShriek,
            SoundCategory::Blocks,
            &pos.to_f64(),
        );
        world.sync_world_event(WorldEvent::ParticlesSculkShriek, *pos, 0);
        world.schedule_block_tick(block, *pos, SHRIEK_TICKS, TickPriority::Normal);

        let mut warning_level = 0;
        if Self::can_respond(world, props.can_summon)
            && let Some(player) = source.and_then(|uuid| Self::player_source(world, uuid))
        {
            let center = pos.to_centered_f64();
            let warden_nearby = world
                .get_nearby_entities(center, 48.0)
                .values()
                .any(|entity| {
                    entity.get_entity().entity_type == &EntityType::WARDEN
                        && (entity.get_entity().pos.load().x - center.x).abs() <= 24.0
                        && (entity.get_entity().pos.load().y - center.y).abs() <= 24.0
                        && (entity.get_entity().pos.load().z - center.z).abs() <= 24.0
                });
            if !warden_nearby {
                let mut tracker = WardenSpawnTracker::read(&player);
                tracker.warn();
                warning_level = tracker.warning_level;
                tracker.write(&player);
            }
        }
        if let Some(entity) = world.get_block_entity(pos)
            && let Some(shrieker) = entity.as_any().downcast_ref::<SculkShriekerBlockEntity>()
        {
            *shrieker
                .warning_level
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = warning_level;
        }
        world.emit_game_event(GameEvent::Shriek.name(), pos.to_centered_f64());
        true
    }

    fn find_spawn_y(origin_y: i32, mut valid: impl FnMut(i32) -> bool) -> Option<i32> {
        (origin_y - WARDEN_SPAWN_VERTICAL_RANGE..=origin_y + WARDEN_SPAWN_VERTICAL_RANGE)
            .rev()
            .find(|&y| valid(y))
    }

    fn has_full_top_collision_face(state: &pumpkin_data::BlockState) -> bool {
        state.get_block_collision_shapes().any(|shape| {
            shape.min.x <= 0.0
                && shape.max.x >= 1.0
                && shape.min.z <= 0.0
                && shape.max.z >= 1.0
                && shape.max.y == 1.0
        })
    }

    fn can_respond(world: &World, can_summon: bool) -> bool {
        let level = world.level_info.load();
        can_summon && level.difficulty != Difficulty::Peaceful && level.game_rules.spawn_wardens
    }

    fn should_respond(can_respond: bool, warning_level: i32) -> bool {
        can_respond && warning_level > 0
    }

    fn try_respond(
        world: &Arc<World>,
        pos: &BlockPos,
        can_summon: bool,
        shrieker: &SculkShriekerBlockEntity,
    ) {
        let warning_level = *shrieker
            .warning_level
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !Self::should_respond(Self::can_respond(world, can_summon), warning_level) {
            return;
        }
        let mut spawned = false;
        if warning_level >= WARN_CAP {
            let base = pos.0;
            let mut rng = rand::rng();
            for _ in 0..WARDEN_SPAWN_ATTEMPTS {
                let x = base.x
                    + rng.random_range(
                        -WARDEN_SPAWN_HORIZONTAL_RANGE..=WARDEN_SPAWN_HORIZONTAL_RANGE,
                    );
                let z = base.z
                    + rng.random_range(
                        -WARDEN_SPAWN_HORIZONTAL_RANGE..=WARDEN_SPAWN_HORIZONTAL_RANGE,
                    );
                let Some(y) = Self::find_spawn_y(base.y, |y| {
                    let feet = BlockPos::new(x, y, z);
                    let below = BlockPos::new(x, y - 1, z);
                    let below_state = world.get_block_state(&below);
                    world
                        .worldborder
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains_block(x, z)
                        && Self::has_full_top_collision_face(below_state)
                        && world.get_block_state(&feet).collision_shapes.is_empty()
                }) else {
                    continue;
                };

                let spawn_pos = pumpkin_util::math::vector3::Vector3::new(
                    f64::from(x) + 0.5,
                    f64::from(y),
                    f64::from(z) + 0.5,
                );
                let warden = from_type(&EntityType::WARDEN, spawn_pos, world, uuid::Uuid::new_v4());
                let bounding_box = warden.get_entity().bounding_box.load();
                // is_space_empty checks blocks only; Pumpkin exposes no matching noCollision check for entities.
                if !world.is_space_empty(bounding_box) || world.contains_any_liquid(bounding_box) {
                    continue;
                }
                spawned = world.try_spawn_entity(warden);
                if spawned {
                    break;
                }
            }
        }
        if warning_level >= WARN_CAP && !spawned {
            world.play_sound(
                Sound::EntityWardenListening,
                SoundCategory::Hostile,
                &pos.to_f64(),
            );
        }
        let center = pos.to_centered_f64();
        let darkness = Effect {
            effect_type: &StatusEffect::DARKNESS,
            duration: DARKNESS_DURATION,
            amplifier: 0,
            ambient: false,
            show_particles: false,
            show_icon: true,
            blend: true,
        };
        for nearby in world.get_nearby_players(center, DARKNESS_RADIUS) {
            nearby.send_effect(&darkness);
            nearby.living_entity.add_effect(darkness.clone());
        }
    }
}

impl BlockBehaviour for SculkShriekerBlock {
    fn on_place(&self, args: OnPlaceArgs<'_>) -> BlockStateId {
        let mut props = SculkShriekerLikeProperties::default(args.block);
        props.shrieking = false;
        props.waterlogged = args.replacing.water_source();
        props.to_state_id(args.block)
    }

    fn broken(&self, args: BrokenArgs<'_>) {
        let props = SculkShriekerLikeProperties::from_state_id(args.state.id);
        if props.shrieking
            && let Some(entity) = args.world.get_block_entity(args.position)
            && let Some(shrieker) = entity.as_any().downcast_ref::<SculkShriekerBlockEntity>()
        {
            Self::try_respond(args.world, args.position, props.can_summon, shrieker);
        }
    }

    fn on_scheduled_tick(&self, args: OnScheduledTickArgs<'_>) {
        let state = args.world.get_block_state(args.position);
        let mut props = SculkShriekerLikeProperties::from_state_id(state.id);
        if props.shrieking {
            props.shrieking = false;
            args.world.set_block_state(
                args.position,
                props.to_state_id(args.block),
                BlockFlags::NOTIFY_ALL,
            );
            if let Some(entity) = args.world.get_block_entity(args.position)
                && let Some(shrieker) = entity.as_any().downcast_ref::<SculkShriekerBlockEntity>()
            {
                Self::try_respond(args.world, args.position, props.can_summon, shrieker);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_y_search_starts_six_above_and_descends_through_six_below() {
        let mut visited = Vec::new();
        assert_eq!(
            SculkShriekerBlock::find_spawn_y(64, |y| {
                visited.push(y);
                y == 60
            }),
            Some(60)
        );
        assert_eq!(visited, [70, 69, 68, 67, 66, 65, 64, 63, 62, 61, 60]);
        assert_eq!(SculkShriekerBlock::find_spawn_y(64, |_| false), None);
    }

    #[test]
    fn response_requires_permission_and_positive_warning() {
        assert!(!SculkShriekerBlock::should_respond(false, 1));
        assert!(!SculkShriekerBlock::should_respond(true, 0));
        assert!(!SculkShriekerBlock::should_respond(true, -1));
        assert!(SculkShriekerBlock::should_respond(true, 1));
    }

    #[test]
    fn tracker_cooldown_and_tick_driven_decay() {
        let mut tracker = WardenSpawnTracker::default();
        assert!(tracker.warn());
        assert_eq!(tracker.cooldown_ticks, 200);
        assert!(!tracker.warn());
        for _ in 0..200 {
            tracker.tick();
        }
        assert_eq!(tracker.cooldown_ticks, 0);
        assert!(tracker.warn());
        assert_eq!(tracker.warning_level, 2);

        let mut decaying = WardenSpawnTracker {
            ticks_since_last_warning: 0,
            warning_level: 2,
            cooldown_ticks: 0,
        };
        for _ in 0..12_000 {
            decaying.tick();
        }
        assert_eq!(decaying.warning_level, 2);
        assert_eq!(decaying.ticks_since_last_warning, 12_000);
        decaying.tick();
        assert_eq!(decaying.warning_level, 1);
        assert_eq!(decaying.ticks_since_last_warning, 0);
    }

    #[test]
    fn tracker_does_not_advance_while_offline() {
        let tracker = WardenSpawnTracker {
            ticks_since_last_warning: 123,
            warning_level: 2,
            cooldown_ticks: 50,
        };
        // Offline time does not call tick, so every relative counter remains unchanged.
        assert_eq!(tracker, WardenSpawnTracker { ..tracker });
    }

    #[test]
    fn tracker_nbt_defaults_clamps_and_roundtrips() {
        let empty = pumpkin_nbt::compound::NbtCompound::new();
        assert_eq!(
            WardenSpawnTracker::from_nbt(&empty),
            WardenSpawnTracker::default()
        );

        let mut invalid = pumpkin_nbt::compound::NbtCompound::new();
        invalid.put_int("ticks_since_last_warning", -1);
        invalid.put_int("warning_level", 99);
        invalid.put_int("cooldown_ticks", -1);
        assert_eq!(
            WardenSpawnTracker::from_nbt(&invalid),
            WardenSpawnTracker {
                ticks_since_last_warning: 0,
                warning_level: WARN_CAP,
                cooldown_ticks: 0,
            }
        );

        let tracker = WardenSpawnTracker {
            ticks_since_last_warning: 50,
            warning_level: 3,
            cooldown_ticks: 250,
        };
        assert_eq!(WardenSpawnTracker::from_nbt(&tracker.to_nbt()), tracker);
    }
}
