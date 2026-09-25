use std::sync::Arc;

use crate::block::entities::calibrated_sculk_sensor::CalibratedSculkSensorBlockEntity;
use crate::block::entities::sculk_sensor::SculkSensorBlockEntity;
use crate::block::{
    BlockBehaviour, BlockMetadata, EmitsRedstonePowerArgs, GetComparatorOutputArgs,
    GetRedstonePowerArgs, OnPlaceArgs, OnScheduledTickArgs, PathComputationType, PlacedArgs,
};
use crate::world::World;
use pumpkin_data::block_properties::{
    CalibratedSculkSensorLikeProperties, SculkSensorLikeProperties, SculkSensorPhase,
};
use pumpkin_data::{Block, BlockId, BlockState, BlockStateId};
use pumpkin_util::math::position::BlockPos;
use pumpkin_world::tick::TickPriority;
use pumpkin_world::world::BlockFlags;

pub struct SculkSensorBlock;

/// Comparator frequency assigned by Mojang's `VibrationSystem`.
/// Events outside `minecraft:vibrations` intentionally return zero.
#[must_use]
pub(crate) const fn vibration_frequency(event: pumpkin_data::game_event::GameEvent) -> i32 {
    use pumpkin_data::game_event::GameEvent as E;
    match event {
        E::Step | E::Swim | E::Flap => 1,
        E::ProjectileLand | E::HitGround | E::Splash | E::Bounce => 2,
        E::ItemInteractFinish | E::ProjectileShoot | E::InstrumentPlay => 3,
        E::EntityAction | E::ElytraGlide | E::Unequip => 4,
        E::EntityDismount | E::Equip => 5,
        E::EntityInteract | E::Shear | E::EntityMount => 6,
        E::EntityDamage => 7,
        E::Drink | E::Eat => 8,
        E::ContainerClose | E::BlockClose | E::BlockDeactivate | E::BlockDetach => 9,
        E::ContainerOpen
        | E::BlockOpen
        | E::BlockActivate
        | E::BlockAttach
        | E::PrimeFuse
        | E::NoteBlockPlay => 10,
        E::BlockChange => 11,
        E::BlockDestroy | E::FluidPickup => 12,
        E::BlockPlace | E::FluidPlace => 13,
        E::EntityPlace | E::LightningStrike | E::Teleport => 14,
        E::EntityDie | E::Explode => 15,
        E::Resonate1 => 1,
        E::Resonate2 => 2,
        E::Resonate3 => 3,
        E::Resonate4 => 4,
        E::Resonate5 => 5,
        E::Resonate6 => 6,
        E::Resonate7 => 7,
        E::Resonate8 => 8,
        E::Resonate9 => 9,
        E::Resonate10 => 10,
        E::Resonate11 => 11,
        E::Resonate12 => 12,
        E::Resonate13 => 13,
        E::Resonate14 => 14,
        E::Resonate15 => 15,
        _ => 0,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VibrationCandidate {
    pub tick: i64,
    pub distance: f64,
    pub frequency: i32,
}

/// Replaces a selector only within the same posting tick; exact ties retain first arrival.
pub(crate) fn select_vibration(
    current: Option<VibrationCandidate>,
    candidate: VibrationCandidate,
) -> Option<VibrationCandidate> {
    match current {
        None => Some(candidate),
        Some(current) if current.tick != candidate.tick => Some(current),
        Some(current)
            if candidate.distance < current.distance
                || (candidate.distance == current.distance
                    && candidate.frequency > current.frequency) =>
        {
            Some(candidate)
        }
        Some(current) => Some(current),
    }
}

#[must_use]
pub(crate) fn vibration_signal(distance: f64, radius: f64) -> u8 {
    (15 - (15.0 / radius * distance).floor() as i32).clamp(1, 15) as u8
}

#[cfg(test)]
mod vibration_tests {
    use super::{VibrationCandidate, select_vibration, vibration_frequency, vibration_signal};
    use pumpkin_data::game_event::GameEvent as E;

    #[test]
    fn vibration_frequency_table_and_unknown_events() {
        assert_eq!(vibration_frequency(E::Step), 1);
        assert_eq!(vibration_frequency(E::Bounce), 2);
        assert_eq!(vibration_frequency(E::ProjectileShoot), 3);
        assert_eq!(vibration_frequency(E::EntityAction), 4);
        assert_eq!(vibration_frequency(E::Equip), 5);
        assert_eq!(vibration_frequency(E::Shear), 6);
        assert_eq!(vibration_frequency(E::EntityDamage), 7);
        assert_eq!(vibration_frequency(E::Eat), 8);
        assert_eq!(vibration_frequency(E::BlockClose), 9);
        assert_eq!(vibration_frequency(E::PrimeFuse), 10);
        assert_eq!(vibration_frequency(E::BlockChange), 11);
        assert_eq!(vibration_frequency(E::FluidPickup), 12);
        assert_eq!(vibration_frequency(E::BlockPlace), 13);
        assert_eq!(vibration_frequency(E::Teleport), 14);
        assert_eq!(vibration_frequency(E::Explode), 15);
        assert_eq!(vibration_frequency(E::Resonate13), 13);
        assert_eq!(vibration_frequency(E::JukeboxPlay), 0);
    }

    #[test]
    fn selector_order_and_exact_ties() {
        let first = VibrationCandidate {
            tick: 4,
            distance: 3.0,
            frequency: 4,
        };
        assert_eq!(select_vibration(None, first).unwrap().frequency, 4);
        let nearer = VibrationCandidate {
            distance: 2.0,
            frequency: 1,
            ..first
        };
        assert_eq!(select_vibration(Some(first), nearer).unwrap().distance, 2.0);
        let freq_tie = VibrationCandidate {
            distance: 3.0,
            frequency: 5,
            ..first
        };
        assert_eq!(
            select_vibration(Some(first), freq_tie).unwrap().frequency,
            5
        );
        let exact_tie = VibrationCandidate {
            frequency: 4,
            ..first
        };
        assert_eq!(
            select_vibration(Some(first), exact_tie).unwrap().frequency,
            4
        );
        let next_tick = VibrationCandidate { tick: 5, ..first };
        assert_eq!(select_vibration(Some(first), next_tick).unwrap().tick, 4);
    }

    #[test]
    fn distance_signal_uses_sensor_radius() {
        assert_eq!(vibration_signal(0.0, 8.0), 15);
        assert_eq!(vibration_signal(4.0, 8.0), 8);
        assert_eq!(vibration_signal(8.0, 8.0), 1);
        assert_eq!(vibration_signal(16.0, 16.0), 1);
        assert_eq!(vibration_signal(30.0, 16.0), 1);
    }
}

impl BlockMetadata for SculkSensorBlock {
    fn ids() -> Box<[BlockId]> {
        [BlockId::SCULK_SENSOR, BlockId::CALIBRATED_SCULK_SENSOR].into()
    }
}

/// Both sensor variants carry the same phase property under different types.
fn sculk_sensor_phase(block: &Block, state_id: BlockStateId) -> SculkSensorPhase {
    if block.id == BlockId::CALIBRATED_SCULK_SENSOR {
        CalibratedSculkSensorLikeProperties::from_state_id(state_id).sculk_sensor_phase
    } else {
        SculkSensorLikeProperties::from_state_id(state_id).sculk_sensor_phase
    }
}

impl SculkSensorBlock {
    pub fn trigger(
        world: &Arc<World>,
        pos: &BlockPos,
        block: &Block,
        power: u8,
        frequency: i32,
    ) -> bool {
        let mut activated = false;
        if block.id == BlockId::SCULK_SENSOR {
            let state = world.get_block_state(pos);
            let mut props = SculkSensorLikeProperties::from_state_id(state.id);
            if props.sculk_sensor_phase == SculkSensorPhase::Inactive {
                if let Some(be) = world.get_block_entity(pos)
                    && let Some(sensor_be) = be.as_any().downcast_ref::<SculkSensorBlockEntity>()
                {
                    *sensor_be
                        .last_vibration_frequency
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = frequency;
                }

                props.sculk_sensor_phase = SculkSensorPhase::Active;
                props.power = power;
                world.set_block_state(pos, props.to_state_id(block), BlockFlags::NOTIFY_ALL);
                world.update_neighbors(pos, None);
                world.schedule_block_tick(block, *pos, 30, TickPriority::Normal);
                activated = true;
            }
        } else if block.id == BlockId::CALIBRATED_SCULK_SENSOR {
            let state = world.get_block_state(pos);
            let mut props = CalibratedSculkSensorLikeProperties::from_state_id(state.id);
            if props.sculk_sensor_phase == SculkSensorPhase::Inactive {
                if let Some(be) = world.get_block_entity(pos)
                    && let Some(cal_be) = be
                        .as_any()
                        .downcast_ref::<CalibratedSculkSensorBlockEntity>()
                {
                    *cal_be
                        .last_vibration_frequency
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = frequency;
                }

                props.sculk_sensor_phase = SculkSensorPhase::Active;
                props.power = power;
                world.set_block_state(pos, props.to_state_id(block), BlockFlags::NOTIFY_ALL);
                world.update_neighbors(pos, None);
                world.schedule_block_tick(block, *pos, 10, TickPriority::Normal);
                activated = true;
            }
        }
        activated
    }
}

impl BlockBehaviour for SculkSensorBlock {
    fn on_place(&self, args: OnPlaceArgs<'_>) -> BlockStateId {
        if args.block.id == BlockId::CALIBRATED_SCULK_SENSOR {
            let mut props = CalibratedSculkSensorLikeProperties::default(args.block);
            props.facing = args.player.living_entity.entity.get_horizontal_facing();
            props.to_state_id(args.block)
        } else {
            let props = SculkSensorLikeProperties::default(args.block);
            props.to_state_id(args.block)
        }
    }

    fn placed(&self, args: PlacedArgs<'_>) {
        if args.block.id == BlockId::CALIBRATED_SCULK_SENSOR {
            let entity = CalibratedSculkSensorBlockEntity::new(*args.position);
            args.world.add_block_entity(Arc::new(entity));
        } else if args.block.id == BlockId::SCULK_SENSOR {
            let entity = SculkSensorBlockEntity::new(*args.position);
            args.world.add_block_entity(Arc::new(entity));
        }
    }

    fn emits_redstone_power(&self, _args: EmitsRedstonePowerArgs<'_>) -> bool {
        true
    }

    fn get_weak_redstone_power(&self, args: GetRedstonePowerArgs<'_>) -> u8 {
        if args.block.id == BlockId::SCULK_SENSOR {
            let props = SculkSensorLikeProperties::from_state_id(args.state.id);
            if props.sculk_sensor_phase == SculkSensorPhase::Active {
                props.power
            } else {
                0
            }
        } else if args.block.id == BlockId::CALIBRATED_SCULK_SENSOR {
            let props = CalibratedSculkSensorLikeProperties::from_state_id(args.state.id);
            if props.sculk_sensor_phase == SculkSensorPhase::Active {
                props.power
            } else {
                0
            }
        } else {
            0
        }
    }

    fn get_comparator_output(&self, args: GetComparatorOutputArgs<'_>) -> Option<u8> {
        // Vanilla reads the frequency only while the sensor is active.
        if sculk_sensor_phase(args.block, args.state.id) != SculkSensorPhase::Active {
            return Some(0);
        }

        let be = args.world.get_block_entity(args.position)?;
        if let Some(sensor_be) = be.as_any().downcast_ref::<SculkSensorBlockEntity>() {
            return Some(
                *sensor_be
                    .last_vibration_frequency
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) as u8,
            );
        }
        if let Some(cal_be) = be
            .as_any()
            .downcast_ref::<CalibratedSculkSensorBlockEntity>()
        {
            return Some(
                *cal_be
                    .last_vibration_frequency
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) as u8,
            );
        }
        None
    }

    fn on_scheduled_tick(&self, args: OnScheduledTickArgs<'_>) {
        let state = args.world.get_block_state(args.position);
        if args.block.id == BlockId::SCULK_SENSOR {
            let mut props = SculkSensorLikeProperties::from_state_id(state.id);
            match props.sculk_sensor_phase {
                SculkSensorPhase::Active => {
                    props.sculk_sensor_phase = SculkSensorPhase::Cooldown;
                    props.power = 0;
                    args.world.set_block_state(
                        args.position,
                        props.to_state_id(args.block),
                        BlockFlags::NOTIFY_ALL,
                    );
                    args.world.schedule_block_tick(
                        args.block,
                        *args.position,
                        10,
                        TickPriority::Normal,
                    );
                }
                SculkSensorPhase::Cooldown => {
                    props.sculk_sensor_phase = SculkSensorPhase::Inactive;
                    props.power = 0;
                    args.world.set_block_state(
                        args.position,
                        props.to_state_id(args.block),
                        BlockFlags::NOTIFY_ALL,
                    );
                }
                SculkSensorPhase::Inactive => {}
            }
        } else if args.block.id == BlockId::CALIBRATED_SCULK_SENSOR {
            let mut props = CalibratedSculkSensorLikeProperties::from_state_id(state.id);
            match props.sculk_sensor_phase {
                SculkSensorPhase::Active => {
                    props.sculk_sensor_phase = SculkSensorPhase::Cooldown;
                    props.power = 0;
                    args.world.set_block_state(
                        args.position,
                        props.to_state_id(args.block),
                        BlockFlags::NOTIFY_ALL,
                    );
                    args.world.schedule_block_tick(
                        args.block,
                        *args.position,
                        10,
                        TickPriority::Normal,
                    );
                }
                SculkSensorPhase::Cooldown => {
                    props.sculk_sensor_phase = SculkSensorPhase::Inactive;
                    props.power = 0;
                    args.world.set_block_state(
                        args.position,
                        props.to_state_id(args.block),
                        BlockFlags::NOTIFY_ALL,
                    );
                }
                SculkSensorPhase::Inactive => {}
            }
        }
    }

    fn is_pathfindable(&self, _state: &BlockState, _computation_type: PathComputationType) -> bool {
        false
    }
}
