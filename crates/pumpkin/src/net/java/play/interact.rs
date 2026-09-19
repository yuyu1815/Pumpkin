use super::attack::{can_use_ordinary_attack_item, valid_entity_interaction};
#[allow(clippy::wildcard_imports)]
use super::*;

impl JavaClient {
    #[expect(clippy::too_many_lines)]
    pub fn handle_interact(
        &self,
        player: &Arc<Player>,
        interact: &SInteract,
        server: &Arc<Server>,
    ) {
        if !player.has_client_loaded() {
            return;
        }
        player.update_last_action_time();
        let entity_id = interact.entity_id;

        let sneaking = interact.sneaking;
        let player_entity = &player.get_entity();
        if player_entity.is_sneaking() != sneaking {
            player_entity.set_sneaking(sneaking);
        }
        let Ok(action) = ActionType::try_from(interact.r#type.0) else {
            self.try_kick(&TextComponent::text("Invalid action type"));
            return;
        };
        if action == ActionType::Attack && entity_id.0 == player.entity_id() {
            self.try_kick(&TextComponent::translate_cross(
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_ENTITY_ATTACKED,
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_ENTITY_ATTACKED,
                [],
            ));
            return;
        }

        let hand = if action == ActionType::Attack {
            None
        } else {
            match interact.hand.map(|hand| hand.0) {
                Some(0) => Some(Hand::Right),
                Some(1) => Some(Hand::Left),
                _ => return,
            }
        };

        // Resolve the target entity for the event
        let world = player_entity.world.load_full();
        let player_target = world.get_player_by_id(entity_id.0);
        let target: Option<Arc<dyn EntityBase>> = player_target
            .as_ref()
            .map(|p| Arc::clone(p) as Arc<dyn EntityBase>)
            .or_else(|| world.get_entity_by_id(entity_id.0));

        if let Some(target) = target {
            let is_attack = action == ActionType::Attack;
            if !valid_entity_interaction(player, &target, is_attack) {
                return;
            }
            let selected_item = if is_attack {
                player.inventory().held_item()
            } else {
                player
                    .inventory()
                    .get_stack_in_hand(hand.expect("entity interaction hand was validated"))
            };
            if !selected_item.is_item_enabled(&server.get_enabled_features())
                || (is_attack && !can_use_ordinary_attack_item(player, &selected_item))
            {
                return;
            }
            if player.gamemode.load() == GameMode::Spectator {
                player.camera_target_id.store(Some(entity_id.0));
                player.try_send_client_packet(&CSetCamera::new(entity_id));
                return;
            }
            send_cancellable_blocking! {{
                server;
                PlayerInteractEntityEvent::new(
                    player,
                    Arc::clone(&target),
                    action,
                    interact.target_position,
                    sneaking,
                );

                'after: {
                    match event.action {
                        ActionType::Attack => {
                            let config = &server.advanced_config.pvp;
                            if !config.enabled {
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
                            player.attack(&event.target);
                        }
                        ActionType::Interact | ActionType::InteractAt => {
                            if event.action == ActionType::InteractAt
                                && let Some(pos) = interact.target_position
                            {
                                let mut at_event = crate::plugin::api::events::player::player_interact_at_entity::PlayerInteractAtEntityEvent::new(
                                    player.clone(),
                                    entity_id.0,
                                    pos.x,
                                    pos.y,
                                    pos.z,
                                    u8::from(hand == Some(Hand::Left)),
                                );
                                server.plugin_manager.fire_blocking(server, &mut at_event);
                                if at_event.cancelled {
                                    return;
                                }
                            }
                            let hand = hand.expect("entity interaction hand was validated");
                            let mut stack = player.inventory().get_stack_in_hand(hand);

                            let item_id = stack.item.id;
                            let before = stack.clone();
                            let interacted = event.target.interact(player, &mut stack);
                            if !interacted {
                                server
                                    .item_registry
                                    .use_on_entity(&mut stack, player, event.target);
                            }
                            if !stack.are_equal(&before) {
                                player.increment_stat(StatisticCategory::Used, item_id as i32, 1);
                                if before.is_damageable() && stack.is_empty() {
                                    player.increment_stat(
                                        StatisticCategory::Broken,
                                        item_id as i32,
                                        1,
                                    );
                                    player.world().send_entity_status(
                                        player.get_entity(),
                                        equipment_break_status(&match hand {
                                            Hand::Right => EquipmentSlot::MAIN_HAND,
                                            Hand::Left => EquipmentSlot::OFF_HAND,
                                        }),
                                        None,
                                    );
                                }
                            }
                            player.inventory().set_stack_in_hand(hand, stack);
                        }
                    }
                }
            }}
        } else if action == ActionType::Attack {
            // An unknown target is invalid for attack, but must not fire an interaction event.
            error!(
                "Player id {} interacted with entity id {}, which was not found.",
                player.entity_id(),
                entity_id.0
            );
            self.try_kick(&TextComponent::translate_cross(
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_ENTITY_ATTACKED,
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_ENTITY_ATTACKED,
                [],
            ));
        }
    }
}
