use super::BlockEntity;
use crate::block::blocks::redstone::sculk_sensor::{SculkSensorBlock, vibration_signal};
use crate::world::World;
use pumpkin_data::game_event::GameEvent;
use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};
use pumpkin_util::math::{position::BlockPos, vector3::Vector3};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
pub(crate) struct PendingVibration {
    pub event: GameEvent,
    pub distance: f64,
    pub position: Vector3<f64>,
    pub delay: u32,
    pub tick: i64,
    pub frequency: i32,
    pub source: Option<uuid::Uuid>,
    pub projectile_owner: Option<uuid::Uuid>,
}

pub struct SculkSensorBlockEntity {
    pub position: BlockPos,
    pub last_vibration_frequency: Mutex<i32>,
    pub(crate) pending_vibration: Mutex<Option<PendingVibration>>,
    pub(crate) selector_vibration: Mutex<Option<PendingVibration>>,
}

impl BlockEntity for SculkSensorBlockEntity {
    fn resource_location(&self) -> &'static str {
        Self::ID
    }
    fn get_position(&self) -> BlockPos {
        self.position
    }
    fn from_nbt(nbt: &NbtCompound, position: BlockPos) -> Self {
        let listener = nbt.get_compound("listener");
        let pending_vibration = listener
            .and_then(|listener| {
                listener
                    .get_compound("event")
                    .map(|event| (listener, event))
            })
            .and_then(|(listener, event)| {
                read_vibration(
                    event,
                    listener.get_int("event_delay").unwrap_or(0).max(0) as u32,
                    -1,
                )
            });
        let selector_vibration = listener
            .and_then(|listener| listener.get_compound("selector"))
            .and_then(|selector| Some((selector, selector.get_compound("event")?)))
            .and_then(|(selector, event)| {
                read_vibration(event, 0, selector.get_long("tick").unwrap_or(-1))
            });
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
        let mut nbt = NbtCompound::new();
        self.write_nbt(&mut nbt);
        Some(nbt)
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
        if let Some(vibration) = *pending {
            if vibration.delay == 0 {
                let block = world.get_block(&self.position);
                SculkSensorBlock::trigger(
                    world,
                    &self.position,
                    block,
                    vibration_signal(vibration.distance, 8.0),
                    vibration.frequency,
                );
                *pending = None;
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
impl SculkSensorBlockEntity {
    pub const ID: &'static str = "minecraft:sculk_sensor";
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

pub(crate) fn write_listener(
    nbt: &mut NbtCompound,
    current: Option<PendingVibration>,
    candidate: Option<PendingVibration>,
) {
    let mut listener = NbtCompound::new();
    let mut selector = NbtCompound::new();
    selector.put_long("tick", candidate.map_or(-1, |v| v.tick));
    if let Some(candidate) = candidate {
        selector.put_compound("event", vibration_nbt(candidate));
    }
    if let Some(current) = current {
        listener.put_compound("event", vibration_nbt(current));
        listener.put_int("event_delay", current.delay as i32);
    } else {
        listener.put_int("event_delay", 0);
    }
    listener.put_compound("selector", selector);
    nbt.put_compound("listener", listener);
}
fn vibration_nbt(v: PendingVibration) -> NbtCompound {
    let mut n = NbtCompound::new();
    n.put_string("game_event", v.event.name().to_string());
    if let Some(source) = v.source {
        n.put_uuid("source", source);
    }
    if let Some(owner) = v.projectile_owner {
        n.put_uuid("projectile_owner", owner);
    }
    n.put_float("distance", v.distance as f32);
    n.put_list(
        "pos",
        vec![
            NbtTag::Double(v.position.x),
            NbtTag::Double(v.position.y),
            NbtTag::Double(v.position.z),
        ],
    );
    n
}
pub(crate) fn read_vibration(n: &NbtCompound, delay: u32, tick: i64) -> Option<PendingVibration> {
    let event = GameEvent::from_name(n.get_string("game_event")?)?;
    let pos = n.get_list("pos")?;
    let [NbtTag::Double(x), NbtTag::Double(y), NbtTag::Double(z)] = pos else {
        return None;
    };
    Some(PendingVibration {
        event,
        distance: f64::from(n.get_float("distance")?),
        position: Vector3::new(*x, *y, *z),
        delay,
        tick,
        frequency: crate::block::blocks::redstone::sculk_sensor::vibration_frequency(event),
        source: n.get_uuid("source"),
        projectile_owner: n.get_uuid("projectile_owner"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_data::game_event::GameEvent;

    #[test]
    fn selector_is_persisted_separately_from_current_event() {
        let position = BlockPos::new(1, 2, 3);
        let sensor = SculkSensorBlockEntity::new(position);
        let uuid = uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210);
        *sensor.selector_vibration.lock().unwrap() = Some(PendingVibration {
            event: GameEvent::Teleport,
            distance: 3.5,
            position: Vector3::new(4.5, 5.5, 6.5),
            delay: 0,
            tick: 9,
            frequency: 14,
            source: Some(uuid),
            projectile_owner: None,
        });
        let mut nbt = NbtCompound::new();
        sensor.write_nbt(&mut nbt);
        let listener = nbt.get_compound("listener").unwrap();
        assert!(listener.get_compound("event").is_none());
        assert_eq!(listener.get_int("event_delay"), Some(0));
        let selector = listener.get_compound("selector").unwrap();
        assert_eq!(selector.get_long("tick"), Some(9));
        assert_eq!(
            selector.get_compound("event").unwrap().get_uuid("source"),
            Some(uuid)
        );
        let restored = SculkSensorBlockEntity::from_nbt(&nbt, position);
        assert!(restored.pending_vibration.lock().unwrap().is_none());
        assert_eq!(
            restored.selector_vibration.lock().unwrap().unwrap().source,
            Some(uuid)
        );
    }

    #[test]
    fn listener_event_survives_block_entity_nbt_roundtrip() {
        let position = BlockPos::new(1, 2, 3);
        let sensor = SculkSensorBlockEntity::new(position);
        *sensor.pending_vibration.lock().unwrap() = Some(PendingVibration {
            event: GameEvent::Step,
            distance: 2.25,
            position: Vector3::new(4.5, 5.5, 6.5),
            delay: 2,
            tick: 7,
            frequency: 1,
            source: None,
            projectile_owner: None,
        });
        let mut nbt = NbtCompound::new();
        sensor.write_nbt(&mut nbt);
        let restored = SculkSensorBlockEntity::from_nbt(&nbt, position);
        let vibration = restored.pending_vibration.lock().unwrap().unwrap();
        assert_eq!(vibration.event, GameEvent::Step);
        assert_eq!(vibration.delay, 2);
        assert_eq!(vibration.position.x, 4.5);
        assert_eq!(
            nbt.get_compound("listener")
                .unwrap()
                .get_compound("selector")
                .unwrap()
                .get_long("tick"),
            Some(7)
        );
    }
}
