use crate::block::{BlockBehaviour, BlockMetadata, OnPlaceArgs, OnScheduledTickArgs};
use pumpkin_data::{
    Block, BlockId, BlockStateId,
    block_properties::SculkCatalystLikeProperties,
    sound::{Sound, SoundCategory},
};
use pumpkin_world::tick::TickPriority;
use pumpkin_world::world::BlockFlags;

pub struct SculkCatalystBlock;

impl BlockMetadata for SculkCatalystBlock {
    fn ids() -> Box<[BlockId]> {
        [BlockId::SCULK_CATALYST].into()
    }
}

impl BlockBehaviour for SculkCatalystBlock {
    fn on_place(&self, args: OnPlaceArgs<'_>) -> BlockStateId {
        let mut props = SculkCatalystLikeProperties::default(args.block);
        props.bloom = false;
        props.to_state_id(args.block)
    }

    fn on_scheduled_tick(&self, args: OnScheduledTickArgs<'_>) {
        let mut props = SculkCatalystLikeProperties::from_state_id(
            args.world.get_block_state(args.position).id,
        );
        if props.bloom {
            props.bloom = false;
            args.world.set_block_state(
                args.position,
                props.to_state_id(&Block::SCULK_CATALYST),
                BlockFlags::NOTIFY_ALL,
            );
        }
    }
}

/// Official Catalyst listener range, measured between block centers.
#[must_use]
pub fn hears_entity_death(
    event: pumpkin_data::game_event::GameEvent,
    has_entity_source: bool,
    distance: f64,
) -> bool {
    event == pumpkin_data::game_event::GameEvent::EntityDie && has_entity_source && distance <= 8.0
}

/// Starts the Catalyst's eight-tick bloom pulse.
pub fn bloom(
    world: &crate::world::World,
    catalyst_position: &pumpkin_util::math::position::BlockPos,
    effect_position: &pumpkin_util::math::position::BlockPos,
) {
    let state = world.get_block_state(catalyst_position);
    let mut props = SculkCatalystLikeProperties::from_state_id(state.id);
    props.bloom = true;
    world.set_block_state(
        catalyst_position,
        props.to_state_id(&Block::SCULK_CATALYST),
        BlockFlags::NOTIFY_ALL,
    );
    world.schedule_block_tick(
        &Block::SCULK_CATALYST,
        *catalyst_position,
        8,
        TickPriority::Normal,
    );
    let effect_position = effect_position.to_centered_f64();
    world.spawn_particles(
        pumpkin_data::particle::Particle::SculkSoul,
        effect_position,
        2,
        pumpkin_util::math::vector3::Vector3::new(0.2, 0.2, 0.2),
        0.0,
    );
    world.play_sound(
        Sound::BlockSculkCatalystBloom,
        SoundCategory::Blocks,
        &catalyst_position.to_centered_f64(),
    );
}

#[cfg(test)]
mod tests {
    use super::hears_entity_death;
    use pumpkin_data::game_event::GameEvent;

    #[test]
    fn catalyst_accepts_only_sourced_entity_deaths_within_eight_blocks() {
        assert!(hears_entity_death(GameEvent::EntityDie, true, 8.0));
        assert!(!hears_entity_death(GameEvent::EntityDie, true, 8.001));
        assert!(!hears_entity_death(GameEvent::EntityDie, false, 1.0));
        assert!(!hears_entity_death(GameEvent::BlockActivate, true, 1.0));
    }
}
