#[allow(clippy::wildcard_imports)]
use super::*;
use pumpkin_data::attributes::Attributes;
use pumpkin_data::data_component_impl::{
    AttackRangeImpl, AttributeModifiersImpl, MinimumAttackChargeImpl, Operation, PiercingWeaponImpl,
};
use pumpkin_util::math::boundingbox::BoundingBox;

const ENTITY_INTERACTION_DISTANCE_VERIFICATION_BUFFER: f64 = 3.0;
const CREATIVE_ENTITY_INTERACTION_RANGE_BONUS: f64 = 2.0;

fn effective_entity_interaction_range(base: f64, creative: bool) -> f64 {
    base + if creative {
        CREATIVE_ENTITY_INTERACTION_RANGE_BONUS
    } else {
        0.0
    }
}

fn valid_entity_target_state(
    same_world: bool,
    removed: bool,
    attack: bool,
    spectator: bool,
    living_health: Option<f32>,
) -> bool {
    same_world
        && !removed
        && (!attack || !spectator)
        && (!attack || living_health.is_none_or(|health| health.is_finite() && health > 0.0))
}

fn squared_distance_to_box(point: Vector3<f64>, bounds: BoundingBox) -> Option<f64> {
    let values = [
        point.x,
        point.y,
        point.z,
        bounds.min.x,
        bounds.min.y,
        bounds.min.z,
        bounds.max.x,
        bounds.max.y,
        bounds.max.z,
    ];
    if values.iter().any(|value| !value.is_finite()) {
        return None;
    }

    let axis_distance = |value: f64, min: f64, max: f64| {
        if value < min {
            min - value
        } else if value > max {
            value - max
        } else {
            0.0
        }
    };
    let dx = axis_distance(point.x, bounds.min.x, bounds.max.x);
    let dy = axis_distance(point.y, bounds.min.y, bounds.max.y);
    let dz = axis_distance(point.z, bounds.min.z, bounds.max.z);
    Some(dx.mul_add(dx, dy.mul_add(dy, dz * dz)))
}

fn is_within_entity_interaction_range(distance_squared: f64, interaction_range: f64) -> bool {
    if !distance_squared.is_finite() || !interaction_range.is_finite() {
        return false;
    }
    let max_range = interaction_range + ENTITY_INTERACTION_DISTANCE_VERIFICATION_BUFFER;
    let max_distance_squared = max_range * max_range;
    max_range.is_finite()
        && max_distance_squared.is_finite()
        && distance_squared < max_distance_squared
}

fn is_within_attack_range(
    distance: f64,
    min_reach: f64,
    max_reach: f64,
    hitbox_margin: f64,
) -> bool {
    if [distance, min_reach, max_reach, hitbox_margin]
        .iter()
        .any(|value| !value.is_finite())
    {
        return false;
    }
    if min_reach < 0.0 || max_reach < 0.0 || hitbox_margin < 0.0 || min_reach > max_reach {
        return false;
    }
    let min_range = min_reach - hitbox_margin - ENTITY_INTERACTION_DISTANCE_VERIFICATION_BUFFER;
    let max_range = max_reach + hitbox_margin + ENTITY_INTERACTION_DISTANCE_VERIFICATION_BUFFER;
    min_range.is_finite() && max_range.is_finite() && distance >= min_range && distance <= max_range
}

fn entity_attack_range(player: &Player, item: &ItemStack) -> Option<(f64, f64, f64)> {
    let creative = player.gamemode.load() == GameMode::Creative;
    if let Some(range) = item.get_data_component::<AttackRangeImpl>() {
        return Some((
            if creative {
                f64::from(range.min_creative_reach)
            } else {
                f64::from(range.min_reach)
            },
            if creative {
                f64::from(range.max_creative_reach)
            } else {
                f64::from(range.max_reach)
            },
            f64::from(range.hitbox_margin),
        ));
    }

    let max_reach = effective_entity_interaction_range(
        player
            .living_entity
            .get_attribute_value(&Attributes::ENTITY_INTERACTION_RANGE),
        creative,
    );
    Some((0.0, max_reach, 0.0))
}

fn item_attack_speed(item: &ItemStack) -> Option<f64> {
    let mut add_speed = 0.0;
    if item.is_empty() {
        // Keep this in lockstep with Player::attack's vanilla fist speed.
        add_speed = -2.4;
    } else if let Some(modifiers) = item.get_data_component::<AttributeModifiersImpl>() {
        for modifier in modifiers.attribute_modifiers.iter() {
            if modifier.operation == Operation::AddValue
                && modifier.id == "minecraft:base_attack_speed"
            {
                add_speed = modifier.amount;
            }
        }
    }

    let attack_speed = 4.0 + add_speed;
    (attack_speed.is_finite() && attack_speed > 0.0).then_some(attack_speed)
}

/// Mirrors `ServerPlayer.cannotAttackWithItem(item, 5)` without mutating the item.
///
/// The official check uses the minimum-attack-charge component and the attack
/// ticker before `Player.attack`; a rejected packet must not reset that ticker or
/// increment the used-item statistic.
#[must_use]
pub(crate) fn can_attack_with_item(player: &Player, item: &ItemStack) -> bool {
    let minimum_charge = item
        .get_data_component::<MinimumAttackChargeImpl>()
        .map_or(0.0, |charge| charge.charge);
    if !minimum_charge.is_finite() || minimum_charge <= 0.0 {
        return minimum_charge.is_finite();
    }

    let Some(attack_speed) = item_attack_speed(item) else {
        return false;
    };
    attack_strength_meets_charge(
        player
            .last_attacked_ticks
            .load(std::sync::atomic::Ordering::Acquire),
        attack_speed,
        minimum_charge,
    )
}

fn attack_strength_meets_charge(ticks: u32, attack_speed: f64, minimum_charge: f32) -> bool {
    let attack_delay = 20.0 / attack_speed;
    let attack_strength = (f64::from(ticks) + 5.0) / attack_delay;
    attack_strength >= f64::from(minimum_charge)
}

#[must_use]
pub(crate) fn can_use_ordinary_attack_item(player: &Player, item: &ItemStack) -> bool {
    can_attack_with_item(player, item) && item.get_data_component::<PiercingWeaponImpl>().is_none()
}

/// Common server-side authority for Java entity interaction packets.
///
/// Vanilla 26.2 validates the target AABB distance, but does not require a block
/// line-of-sight raycast for these packets. `attack` additionally follows the
/// attackable-target rule; interaction keeps the broader entity interaction path.
#[must_use]
pub(crate) fn valid_entity_interaction(
    player: &Arc<Player>,
    target: &Arc<dyn EntityBase>,
    attack: bool,
) -> bool {
    let player_entity = player.get_entity();
    let target_entity = target.get_entity();
    let player_world = player_entity.world.load_full();
    let target_world = target_entity.world.load_full();
    let living_health = target
        .get_living_entity()
        .map(|living| living.health.load());
    if !valid_entity_target_state(
        Arc::ptr_eq(&player_world, &target_world),
        target_entity.is_removed(),
        attack,
        target.is_spectator(),
        living_health,
    ) {
        return false;
    }
    if attack
        && (player.is_spectator() || target.get_living_entity().is_none() && !target.can_hit())
    {
        return false;
    }
    let block_pos = target_entity.block_pos.load();
    if !player_world
        .worldborder
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_block(block_pos.0.x, block_pos.0.z)
    {
        return false;
    }

    let bounds = target_entity.bounding_box.load();
    let Some(distance_squared) = squared_distance_to_box(player.eye_position(), bounds) else {
        return false;
    };

    if attack {
        let item = player.inventory().held_item();
        let Some((min_reach, max_reach, hitbox_margin)) = entity_attack_range(player, &item) else {
            return false;
        };
        is_within_attack_range(distance_squared.sqrt(), min_reach, max_reach, hitbox_margin)
    } else {
        let interaction_range = effective_entity_interaction_range(
            player
                .living_entity
                .get_attribute_value(&Attributes::ENTITY_INTERACTION_RANGE),
            player.gamemode.load() == GameMode::Creative,
        );
        is_within_entity_interaction_range(distance_squared, interaction_range)
    }
}

impl JavaClient {
    pub fn handle_attack(&self, player: &Arc<Player>, attack: &SAttack, server: &Arc<Server>) {
        if !player.has_client_loaded() || player.is_spectator() {
            return;
        }
        player.update_last_action_time();
        let entity_id = attack.entity_id;
        let player_entity = &player.get_entity();
        let world = player_entity.world.load_full();

        let config = &server.advanced_config.pvp;
        if !config.enabled {
            return;
        }

        if entity_id.0 == player.entity_id() {
            self.try_kick(&TextComponent::translate_cross(
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_ENTITY_ATTACKED,
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_ENTITY_ATTACKED,
                [],
            ));
            return;
        }

        let player_target = world.get_player_by_id(entity_id.0);
        let target: Option<Arc<dyn EntityBase>> = player_target
            .as_ref()
            .map(|p| Arc::clone(p) as Arc<dyn EntityBase>)
            .or_else(|| world.get_entity_by_id(entity_id.0));
        let Some(target) = target else {
            self.try_kick(&TextComponent::translate_cross(
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_ENTITY_ATTACKED,
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_ENTITY_ATTACKED,
                [],
            ));
            return;
        };
        if !valid_entity_interaction(player, &target, true) {
            return;
        }
        let item = player.inventory().held_item();
        if !item.is_item_enabled(&server.get_enabled_features())
            || !can_use_ordinary_attack_item(player, &item)
        {
            return;
        }
        if let Some(player_victim) = &player_target
            && config.protect_creative
            && player_victim.gamemode.load() == GameMode::Creative
        {
            world.play_sound(
                Sound::EntityPlayerAttackNodamage,
                SoundCategory::Players,
                &player_victim.position(),
            );
            return;
        }
        player.attack(&target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attack_range_uses_inclusive_outer_boundary_and_target_box_distance() {
        let target_box = BoundingBox {
            min: Vector3::new(2.0, 2.0, 2.0),
            max: Vector3::new(4.0, 4.0, 4.0),
        };
        assert_eq!(
            squared_distance_to_box(Vector3::new(1.0, 3.0, 3.0), target_box),
            Some(1.0)
        );
        assert_eq!(
            squared_distance_to_box(Vector3::new(3.0, 3.0, 3.0), target_box),
            Some(0.0)
        );
        assert!(is_within_attack_range(6.0, 0.0, 3.0, 0.0));
        assert!(!is_within_attack_range(6.0 + 1.0e-9, 0.0, 3.0, 0.0));
        assert!(is_within_attack_range(7.5, 2.0, 4.5, 0.125));
    }

    #[test]
    fn interaction_range_uses_strict_outer_boundary() {
        assert!(is_within_entity_interaction_range(36.0 - 1.0e-9, 3.0));
        assert!(!is_within_entity_interaction_range(36.0, 3.0));
    }

    #[test]
    fn creative_and_attribute_overrides_change_the_authoritative_range() {
        assert_eq!(effective_entity_interaction_range(3.0, false), 3.0);
        assert_eq!(effective_entity_interaction_range(3.0, true), 5.0);
        assert!(is_within_attack_range(
            8.0,
            0.0,
            effective_entity_interaction_range(3.0, true),
            0.0,
        ));
        assert!(!is_within_attack_range(
            8.0 + 1.0e-9,
            0.0,
            effective_entity_interaction_range(3.0, true),
            0.0,
        ));
        assert!(is_within_attack_range(
            7.0,
            0.0,
            effective_entity_interaction_range(4.0, false),
            0.0,
        ));
        assert!(!is_within_attack_range(
            7.0 + 1.0e-9,
            0.0,
            effective_entity_interaction_range(4.0, false),
            0.0,
        ));
        assert!(is_within_entity_interaction_range(
            49.0 - 1.0e-9,
            effective_entity_interaction_range(4.0, false),
        ));
        assert!(!is_within_entity_interaction_range(
            49.0,
            effective_entity_interaction_range(4.0, false),
        ));
    }

    #[test]
    fn non_finite_range_inputs_are_rejected() {
        assert!(!is_within_attack_range(f64::NAN, 0.0, 3.0, 0.0));
        assert!(!is_within_attack_range(1.0, 0.0, f64::INFINITY, 0.0));
        assert!(!is_within_entity_interaction_range(f64::NAN, 3.0));
        assert!(!is_within_entity_interaction_range(1.0, f64::NAN));
        assert!(!is_within_entity_interaction_range(1.0, f64::MAX));
        assert!(!is_within_attack_range(1.0, 4.0, 3.0, 0.0));
    }

    #[test]
    fn minimum_attack_charge_is_a_pre_attack_guard() {
        assert!(attack_strength_meets_charge(0, 4.0, 1.0));
        assert!(!attack_strength_meets_charge(0, 1.6, 1.0));
        assert!(attack_strength_meets_charge(8, 1.6, 1.0));
    }

    #[test]
    fn removed_and_dead_targets_are_rejected_before_side_effects() {
        assert!(!valid_entity_target_state(
            true,
            true,
            true,
            false,
            Some(20.0)
        ));
        assert!(!valid_entity_target_state(
            true,
            false,
            true,
            false,
            Some(0.0)
        ));
        assert!(!valid_entity_target_state(true, false, true, true, None));
        assert!(valid_entity_target_state(
            true,
            false,
            false,
            true,
            Some(0.0)
        ));
        assert!(!valid_entity_target_state(
            false,
            false,
            true,
            false,
            Some(20.0)
        ));
        assert!(valid_entity_target_state(
            true,
            false,
            true,
            false,
            Some(20.0)
        ));
    }
}
