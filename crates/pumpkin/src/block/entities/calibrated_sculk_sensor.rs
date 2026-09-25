use super::BlockEntity;
use crate::block::blocks::redstone::sculk_sensor::{SculkSensorBlock, vibration_signal};
use crate::block::entities::sculk_sensor::{PendingVibration, read_vibration, write_listener};
use crate::world::World;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_util::math::position::BlockPos;
use std::sync::{Arc, Mutex};

pub struct CalibratedSculkSensorBlockEntity {
    pub position: BlockPos,
    pub last_vibration_frequency: Mutex<i32>,
    pub(crate) pending_vibration: Mutex<Option<PendingVibration>>,
    pub(crate) selector_vibration: Mutex<Option<PendingVibration>>,
}
impl BlockEntity for CalibratedSculkSensorBlockEntity {
    fn resource_location(&self) -> &'static str {
        Self::ID
    }
    fn get_position(&self) -> BlockPos {
        self.position
    }
    fn from_nbt(nbt: &NbtCompound, position: BlockPos) -> Self {
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
            last_vibration_frequency: Mutex::new(
                nbt.get_int("last_vibration_frequency").unwrap_or(0),
            ),
            pending_vibration: Mutex::new(pending_vibration),
            selector_vibration: Mutex::new(selector_vibration),
        }
    }
    fn write_nbt(&self, nbt: &mut NbtCompound) {
        nbt.put_int(
            "last_vibration_frequency",
            *self
                .last_vibration_frequency
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
        let mut n = NbtCompound::new();
        self.write_nbt(&mut n);
        Some(n)
    }
    fn tick(&self, world: &Arc<World>) {
        let mut pending = self
            .pending_vibration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = world.get_world_age();
        let mut selector = self
            .selector_vibration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(candidate) = *selector {
            if candidate.tick < now {
                *pending = Some(PendingVibration {
                    delay: candidate.distance.floor() as u32,
                    ..candidate
                });
                *selector = None;
            }
        }
        if let Some(v) = *pending {
            if v.delay == 0 {
                let block = world.get_block(&self.position);
                let activated = SculkSensorBlock::trigger(
                    world,
                    &self.position,
                    block,
                    vibration_signal(v.distance, 16.0),
                    v.frequency,
                );
                *pending = None;
                drop(selector);
                drop(pending);
                if activated {
                    world.emit_game_event_with_source(
                        pumpkin_data::game_event::GameEvent::SculkSensorTendrilsClicking.name(),
                        self.position.to_centered_f64(),
                        v.source.and_then(|uuid| {
                            world.sculk_source_for_vibration(uuid, v.projectile_owner)
                        }),
                        None,
                    );
                }
            } else {
                *pending = Some(PendingVibration {
                    delay: v.delay - 1,
                    ..v
                });
            }
        }
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
impl CalibratedSculkSensorBlockEntity {
    pub const ID: &'static str = "minecraft:calibrated_sculk_sensor";
    #[must_use]
    pub const fn new(position: BlockPos) -> Self {
        Self {
            position,
            last_vibration_frequency: Mutex::new(0),
            pending_vibration: Mutex::new(None),
            selector_vibration: Mutex::new(None),
        }
    }
}
