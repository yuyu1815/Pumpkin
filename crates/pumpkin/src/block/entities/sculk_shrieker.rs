use super::BlockEntity;
use crate::block::blocks::sculk::sculk_shrieker::SculkShriekerBlock;
use crate::block::entities::sculk_sensor::{PendingVibration, read_vibration, write_listener};
use crate::world::World;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_util::math::position::BlockPos;
use std::sync::{Arc, Mutex};

pub struct SculkShriekerBlockEntity {
    pub position: BlockPos,
    pub warning_level: Mutex<i32>,
    pub(crate) pending_vibration: Mutex<Option<PendingVibration>>,
    pub(crate) selector_vibration: Mutex<Option<PendingVibration>>,
}

impl BlockEntity for SculkShriekerBlockEntity {
    fn resource_location(&self) -> &'static str {
        Self::ID
    }

    fn get_position(&self) -> BlockPos {
        self.position
    }

    fn from_nbt(nbt: &pumpkin_nbt::compound::NbtCompound, position: BlockPos) -> Self
    where
        Self: Sized,
    {
        let warning_level = nbt.get_int("warning_level").unwrap_or(0);
        let listener = nbt.get_compound("listener");
        let pending_vibration = listener
            .and_then(|l| l.get_compound("event").map(|e| (l, e)))
            .and_then(|(l, e)| {
                read_vibration(e, l.get_int("event_delay").unwrap_or(0).max(0) as u32, -1)
            });
        let selector_vibration = listener
            .and_then(|l| l.get_compound("selector"))
            .and_then(|s| Some((s, s.get_compound("event")?)))
            .and_then(|(s, e)| read_vibration(e, 0, s.get_long("tick").unwrap_or(-1)));
        Self {
            position,
            warning_level: Mutex::new(warning_level),
            pending_vibration: Mutex::new(pending_vibration),
            selector_vibration: Mutex::new(selector_vibration),
        }
    }

    fn write_nbt(&self, nbt: &mut NbtCompound) {
        nbt.put_int(
            "warning_level",
            *self
                .warning_level
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        write_listener(
            nbt,
            *self
                .pending_vibration
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            *self
                .selector_vibration
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }

    fn chunk_data_nbt(&self) -> Option<NbtCompound> {
        let mut nbt = NbtCompound::new();
        self.write_nbt(&mut nbt);
        Some(nbt)
    }

    fn tick(&self, world: &Arc<World>) {
        let mut pending = self
            .pending_vibration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut selector = self
            .selector_vibration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(candidate) = *selector
            && candidate.tick < world.get_world_age()
        {
            *pending = Some(PendingVibration {
                delay: candidate.distance.floor() as u32,
                ..candidate
            });
            *selector = None;
        }
        if let Some(vibration) = *pending {
            if vibration.delay == 0 {
                *pending = None;
                drop(selector);
                drop(pending);
                let source = vibration.source.filter(|uuid| {
                    world
                        .get_entity_by_uuid(*uuid)
                        .is_some_and(|entity| entity.get_item_entity().is_some())
                });
                SculkShriekerBlock::try_activate(
                    world,
                    &self.position,
                    source.or(vibration.projectile_owner).or(vibration.source),
                );
            } else {
                *pending = Some(PendingVibration {
                    delay: vibration.delay - 1,
                    ..vibration
                });
            }
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl SculkShriekerBlockEntity {
    pub const ID: &'static str = "minecraft:sculk_shrieker";
    #[must_use]
    pub const fn new(position: BlockPos) -> Self {
        Self {
            position,
            warning_level: Mutex::new(0),
            pending_vibration: Mutex::new(None),
            selector_vibration: Mutex::new(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::entities::sculk_sensor::PendingVibration;
    use pumpkin_data::game_event::GameEvent;
    use pumpkin_util::math::vector3::Vector3;

    #[test]
    fn vibration_and_warning_state_survive_nbt_roundtrip() {
        let position = BlockPos::new(1, 2, 3);
        let shrieker = SculkShriekerBlockEntity::new(position);
        let source = uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210);
        *shrieker.warning_level.lock().unwrap() = 2;
        *shrieker.pending_vibration.lock().unwrap() = Some(PendingVibration {
            event: GameEvent::Step,
            distance: 2.25,
            position: Vector3::new(4.5, 5.5, 6.5),
            delay: 2,
            tick: -1,
            frequency: 1,
            source: Some(source),
            projectile_owner: None,
        });
        *shrieker.selector_vibration.lock().unwrap() = Some(PendingVibration {
            event: GameEvent::Teleport,
            distance: 3.5,
            position: Vector3::new(7.5, 8.5, 9.5),
            delay: 0,
            tick: 9,
            frequency: 14,
            source: None,
            projectile_owner: None,
        });
        let mut nbt = NbtCompound::new();
        shrieker.write_nbt(&mut nbt);

        let restored = SculkShriekerBlockEntity::from_nbt(&nbt, position);
        assert_eq!(*restored.warning_level.lock().unwrap(), 2);
        let vibration = restored.pending_vibration.lock().unwrap().unwrap();
        assert_eq!(vibration.event, GameEvent::Step);
        assert_eq!(vibration.delay, 2);
        assert_eq!(vibration.source, Some(source));
        let selector = restored.selector_vibration.lock().unwrap().unwrap();
        assert_eq!(selector.event, GameEvent::Teleport);
        assert_eq!(selector.tick, 9);
    }
}
