use std::sync::Arc;

use pumpkin_data::game_event::GameEvent;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector3::Vector3;

use super::EnchantmentEntityEffectExt;
use crate::entity::player::Player;
use crate::entity::{Entity, EntityBase};
use crate::world::World;

/// Enchantment entity effect that replaces a block at an offset position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReplaceBlock {
    pub offset: Vector3<i32>,
    pub trigger_game_event: Option<GameEvent>,
}

impl ReplaceBlock {
    #[must_use]
    pub const fn new(offset: Vector3<i32>, trigger_game_event: Option<GameEvent>) -> Self {
        Self {
            offset,
            trigger_game_event,
        }
    }

    #[must_use]
    pub const fn target_position(&self, origin: Vector3<f64>) -> BlockPos {
        let base_x = origin.x.floor() as i32;
        let base_y = origin.y.floor() as i32;
        let base_z = origin.z.floor() as i32;
        BlockPos::new(
            base_x + self.offset.x,
            base_y + self.offset.y,
            base_z + self.offset.z,
        )
    }
}

impl EnchantmentEntityEffectExt for ReplaceBlock {
    fn apply(
        &self,
        world: &Arc<World>,
        _enchantment_level: i32,
        _owner: Option<&Arc<Player>>,
        entity: Option<&Entity>,
        position: Vector3<f64>,
    ) {
        let target = self.target_position(position);
        if let Some(event) = self.trigger_game_event {
            let source = entity.and_then(|entity| {
                let uuid = entity.entity_uuid;
                world
                    .get_entity_by_uuid(uuid)
                    .map(|source| crate::world::SculkEventSource {
                        uuid: source.get_entity().entity_uuid,
                        projectile_owner: crate::entity::projectile::is_projectile(
                            source.get_entity().entity_type,
                        )
                        .then(|| source.get_owner_id())
                        .flatten()
                        .and_then(|owner_id| world.get_entity_by_id(owner_id))
                        .map(|owner| owner.get_entity().entity_uuid),
                        spectator: source.is_spectator(),
                        sneaking: source.get_entity().is_sneaking(),
                        dampens_vibrations: source.dampens_vibrations(),
                    })
            });
            world.emit_game_event_with_source(event.name(), target.to_centered_f64(), source, None);
        }
    }
}
