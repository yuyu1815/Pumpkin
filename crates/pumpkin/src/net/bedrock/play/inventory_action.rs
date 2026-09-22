#[allow(clippy::wildcard_imports)]
use super::*;
use crate::item::registry::should_try_block_placement;
use pumpkin_inventory::slot::Slot;

fn checked_descriptor_to_stack(desc: &NetworkItemDescriptor) -> Option<ItemStack> {
    if desc.stack_size > u16::from(u8::MAX)
        || (desc.id.0 == 0) != (desc.stack_size == 0)
        || (!desc.nbt_data.is_empty()
            || !desc.place_on_blocks.is_empty()
            || !desc.destroy_blocks.is_empty()
            || desc.shield_blocking_tick != 0)
    {
        return None;
    }

    let stack = descriptor_to_stack(desc);
    if desc.id.0 != 0
        && (stack.is_empty()
            || stack.item_count != desc.stack_size as u8
            || stack.item_count > stack.get_max_stack_size())
    {
        return None;
    }
    Some(stack)
}

fn descriptor_matches_stack(desc: &NetworkItemDescriptor, current: &ItemStack) -> bool {
    let expected = NetworkItemDescriptor::from(current);
    checked_descriptor_to_stack(desc).is_some_and(|stack| {
        stack.item == current.item
            && stack.item_count == current.item_count
            && desc.id == expected.id
            && desc.aux_value == expected.aux_value
            && desc.block_runtime_id == expected.block_runtime_id
    })
}

fn slot_allows_legacy_update(
    player: Option<&dyn InventoryPlayer>,
    slot: &dyn Slot,
    current: &ItemStack,
    stack: &ItemStack,
) -> bool {
    (current.is_empty()
        || current.are_equal(stack)
        || player.is_none_or(|player| slot.can_take_items(player)))
        && (stack.is_empty()
            || (slot.can_insert(stack)
                && stack.item_count <= slot.get_max_item_count_for_stack(stack)))
}

struct LegacySlotState {
    screen_slot: usize,
    stack: ItemStack,
    slot: Arc<dyn Slot>,
}

struct LegacyPlan {
    updates: Vec<(usize, ItemStack)>,
    drops: Vec<ItemStack>,
}

pub(crate) fn commit_held_item(player: &Arc<Player>, stack: ItemStack) {
    player.inventory().set_held_item(stack.clone());
    let screen_slot = 36 + player.inventory().get_selected_slot() as usize;
    let mut player_screen_handler = player
        .player_screen_handler
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    player_screen_handler.set_received_stack(screen_slot, stack);
    player_screen_handler.send_content_updates();
}

fn resync_current_screen_handler(player: &Player) {
    let current = player
        .current_screen_handler
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    current
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .update_to_client();
}

fn add_stack_count(counts: &mut Vec<(ItemStack, u32)>, stack: &ItemStack, amount: u8) {
    if stack.is_empty() || amount == 0 {
        return;
    }
    if let Some((_, count)) = counts
        .iter_mut()
        .find(|(existing, _)| existing.are_items_and_components_equal(stack))
    {
        *count += u32::from(amount);
    } else {
        counts.push((stack.clone(), u32::from(amount)));
    }
}

fn counts_match(
    before: &[LegacySlotState],
    after: &[(usize, ItemStack)],
    dropped: &[ItemStack],
) -> bool {
    let mut before_counts: Vec<(ItemStack, u32)> = Vec::new();
    let mut after_counts: Vec<(ItemStack, u32)> = Vec::new();
    for state in before {
        add_stack_count(&mut before_counts, &state.stack, state.stack.item_count);
    }
    for (_, stack) in after {
        add_stack_count(&mut after_counts, stack, stack.item_count);
    }
    for stack in dropped {
        add_stack_count(&mut after_counts, stack, stack.item_count);
    }
    before_counts.iter().all(|(stack, count)| {
        after_counts.iter().any(|(other, other_count)| {
            other.are_items_and_components_equal(stack) && other_count == count
        })
    }) && after_counts.iter().all(|(stack, count)| {
        before_counts.iter().any(|(other, other_count)| {
            other.are_items_and_components_equal(stack) && other_count == count
        })
    })
}

fn validate_legacy_container_actions(
    player: Option<&dyn InventoryPlayer>,
    actions: &[pumpkin_protocol::bedrock::server::inventory_transaction::InventoryAction],
    states: &[LegacySlotState],
) -> Option<LegacyPlan> {
    use pumpkin_protocol::bedrock::server::inventory_transaction::InventoryActionSource;

    let mut after = Vec::with_capacity(states.len());
    let mut dropped = Vec::new();
    let mut seen = Vec::new();

    for action in actions {
        match InventoryActionSource::from(action.source_type) {
            InventoryActionSource::Container => {}
            InventoryActionSource::World => {
                if action.window_id.is_some()
                    || !checked_descriptor_to_stack(&action.old_item)?.is_empty()
                {
                    return None;
                }
                let dropped_stack = checked_descriptor_to_stack(&action.new_item)?;
                if dropped_stack.is_empty() {
                    return None;
                }
                dropped.push(dropped_stack);
                continue;
            }
            InventoryActionSource::Creative
            | InventoryActionSource::Todo
            | InventoryActionSource::Unknown(_) => return None,
        }

        let screen_slot =
            map_bedrock_slot_to_screen_handler(action.window_id?, action.inventory_slot)?;
        let state = states
            .iter()
            .find(|state| state.screen_slot == screen_slot)?;
        if seen.contains(&screen_slot)
            || !state.stack.patch.is_empty()
            || !descriptor_matches_stack(&action.old_item, &state.stack)
        {
            return None;
        }
        seen.push(screen_slot);
        let new_stack = checked_descriptor_to_stack(&action.new_item)?;
        let new_stack = if new_stack.is_empty() {
            ItemStack::EMPTY.clone()
        } else {
            let donor = states.iter().find(|candidate| {
                !candidate.stack.is_empty()
                    && candidate.stack.patch.is_empty()
                    && candidate.stack.item == new_stack.item
            })?;
            donor.stack.copy_with_count(new_stack.item_count)
        };
        if !slot_allows_legacy_update(player, state.slot.as_ref(), &state.stack, &new_stack) {
            return None;
        }
        after.push((screen_slot, new_stack));
    }

    if !counts_match(states, &after, &dropped) {
        return None;
    }
    Some(LegacyPlan {
        updates: after,
        drops: dropped,
    })
}

fn legacy_action_shape(
    gamemode: GameMode,
    action: &pumpkin_protocol::bedrock::server::inventory_transaction::InventoryAction,
) -> bool {
    use pumpkin_protocol::bedrock::server::inventory_transaction::InventoryActionSource;

    match InventoryActionSource::from(action.source_type) {
        InventoryActionSource::Container | InventoryActionSource::Creative => {
            let source_allowed = match InventoryActionSource::from(action.source_type) {
                InventoryActionSource::Container => gamemode != GameMode::Spectator,
                InventoryActionSource::Creative => gamemode == GameMode::Creative,
                _ => false,
            };
            source_allowed
                && action
                    .window_id
                    .and_then(|window| {
                        map_bedrock_slot_to_screen_handler(window, action.inventory_slot)
                    })
                    .is_some()
                && checked_descriptor_to_stack(&action.old_item).is_some()
                && checked_descriptor_to_stack(&action.new_item).is_some()
        }
        InventoryActionSource::World => {
            action.window_id.is_none()
                && checked_descriptor_to_stack(&action.old_item)
                    .is_some_and(|stack| stack.is_empty())
                && checked_descriptor_to_stack(&action.new_item)
                    .is_some_and(|stack| !stack.is_empty())
        }
        InventoryActionSource::Todo | InventoryActionSource::Unknown(_) => false,
    }
}

fn validate_legacy_creative_action(
    gamemode: GameMode,
    player: &dyn InventoryPlayer,
    action: &pumpkin_protocol::bedrock::server::inventory_transaction::InventoryAction,
    current: &ItemStack,
    slot: &dyn Slot,
) -> Option<ItemStack> {
    use pumpkin_protocol::bedrock::server::inventory_transaction::InventoryActionSource;

    if gamemode != GameMode::Creative
        || !matches!(
            InventoryActionSource::from(action.source_type),
            InventoryActionSource::Container | InventoryActionSource::Creative
        )
        || action.window_id.is_none()
        || !descriptor_matches_stack(&action.old_item, current)
    {
        return None;
    }
    let new_stack = checked_descriptor_to_stack(&action.new_item)?;
    slot_allows_legacy_update(Some(player), slot, current, &new_stack).then_some(new_stack)
}

impl BedrockClient {
    fn correct_rejected_food_use(&self, player: &Player) {
        // Holding use can repeat rejected transactions. Limit prediction corrections
        // to every 20 ticks, without indefinitely trusting a previously sent snapshot.
        const CORRECTION_INTERVAL_TICKS: i32 = 20;
        let tick = player.tick_counter.load(Ordering::Relaxed);
        let recently_corrected = self.last_food_rejection_tick.load().is_some_and(|last| {
            tick >= last && tick.saturating_sub(last) < CORRECTION_INTERVAL_TICKS
        });
        let has_active_use = player
            .living_entity
            .item_in_use
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        if recently_corrected && !has_active_use {
            return;
        }
        if player.has_client_loaded() {
            self.last_food_rejection_tick.store(Some(tick));
        }
        player.living_entity.clear_active_hand();
        player.send_health();
    }

    #[allow(clippy::too_many_lines, clippy::collapsible_if, clippy::unreachable)]
    pub fn handle_inventory_action(&self, player: &Arc<Player>, packet: SInventoryTransaction) {
        tracing::debug!("handle_inventory_action: packet={:?}", packet);
        let mut inventory_updated = false;
        let mut updates = Vec::new();
        let mut result = 0u8;
        let mut rejected_action = false;
        let action_transaction = matches!(
            &packet.transaction_data,
            TransactionData::Normal(_) | TransactionData::Mismatch(_)
        );
        let legacy_slots_valid = packet.legacy_set_item_slots.iter().all(|legacy_slot| {
            let mapped_window_id = match legacy_slot.container_id {
                28 | 29 => 0,
                6 | 120 => 120,
                34 | 119 => 119,
                _ => return false,
            };
            legacy_slot.slots.iter().all(|&slot_id| {
                map_bedrock_slot_to_screen_handler(mapped_window_id, slot_id as u32).is_some()
            })
        });

        if action_transaction
            && packet.actions.is_empty()
            && packet.legacy_request_id.0 != 0
            && legacy_slots_valid
        {
            let mut player_screen_handler = player
                .player_screen_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if packet.legacy_set_item_slots.iter().all(|legacy_slot| {
                let mapped_window_id = match legacy_slot.container_id {
                    28 | 29 => 0,
                    6 | 120 => 120,
                    34 | 119 => 119,
                    _ => return false,
                };
                legacy_slot.slots.iter().all(|&slot_id| {
                    map_bedrock_slot_to_screen_handler(mapped_window_id, slot_id as u32)
                        .is_some_and(|screen_slot| {
                            player_screen_handler
                                .get_slot(screen_slot)
                                .can_take_items(player.as_ref())
                        })
                })
            }) {
                for legacy_slot in &packet.legacy_set_item_slots {
                    let mapped_window_id = match legacy_slot.container_id {
                        28 | 29 => 0,    // HotBar or Inventory
                        6 | 120 => 120,  // Armor
                        34 | 119 => 119, // Offhand
                        other => other as i32,
                    };
                    for &slot_id in &legacy_slot.slots {
                        if let Some(screen_slot) =
                            map_bedrock_slot_to_screen_handler(mapped_window_id, slot_id as u32)
                        {
                            let current_stack = player_screen_handler
                                .get_slot(screen_slot)
                                .get_cloned_stack();
                            if !current_stack.is_empty() {
                                player.drop_item(current_stack.clone());

                                player_screen_handler
                                    .get_slot(screen_slot)
                                    .set_stack(ItemStack::EMPTY.clone());
                                player_screen_handler
                                    .set_received_stack(screen_slot, ItemStack::EMPTY.clone());

                                record_update(
                                    &mut updates,
                                    FullContainerName {
                                        container_name: match legacy_slot.container_id {
                                            28 => ContainerName::HotBar,
                                            _ => ContainerName::Inventory,
                                        },
                                        dynamic_id: None,
                                    },
                                    slot_id,
                                    ItemStack::EMPTY,
                                );
                                inventory_updated = true;
                            }
                        }
                    }
                }
            } else {
                rejected_action = true;
                result = 1;
                inventory_updated = true;
            }
            player_screen_handler.send_content_updates();
            drop(player_screen_handler);
        } else if action_transaction && packet.actions.is_empty() && packet.legacy_request_id.0 != 0
        {
            rejected_action = true;
            result = 1;
            inventory_updated = true;
        }

        let gamemode = player.gamemode.load();
        if action_transaction
            && !packet
                .actions
                .iter()
                .all(|action| legacy_action_shape(gamemode, action))
        {
            rejected_action = true;
            result = 1;
            inventory_updated = true;
        }

        if !rejected_action && action_transaction && !packet.actions.is_empty() {
            let mut player_screen_handler = player
                .player_screen_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut states = Vec::new();
            for action in &packet.actions {
                if let Some(window_id) = action.window_id
                    && let Some(screen_slot) =
                        map_bedrock_slot_to_screen_handler(window_id, action.inventory_slot)
                    && pumpkin_protocol::bedrock::server::inventory_transaction::InventoryActionSource::from(
                        action.source_type,
                    ) != pumpkin_protocol::bedrock::server::inventory_transaction::InventoryActionSource::World
                {
                    let slot = player_screen_handler.get_slot(screen_slot);
                    states.push(LegacySlotState {
                        screen_slot,
                        stack: slot.get_cloned_stack(),
                        slot,
                    });
                }
            }

            if gamemode == GameMode::Spectator {
                rejected_action = true;
            } else if gamemode == GameMode::Creative {
                let mut updates = Vec::new();
                for action in &packet.actions {
                    let Some(window_id) = action.window_id else {
                        rejected_action = true;
                        break;
                    };
                    let Some(screen_slot) =
                        map_bedrock_slot_to_screen_handler(window_id, action.inventory_slot)
                    else {
                        rejected_action = true;
                        break;
                    };
                    let slot = player_screen_handler.get_slot(screen_slot);
                    let current_stack = slot.get_cloned_stack();
                    let Some(item_stack) = validate_legacy_creative_action(
                        gamemode,
                        player.as_ref(),
                        action,
                        &current_stack,
                        slot.as_ref(),
                    ) else {
                        rejected_action = true;
                        break;
                    };
                    updates.push((screen_slot, item_stack));
                }
                if !rejected_action {
                    for (screen_slot, item_stack) in updates {
                        player_screen_handler
                            .get_slot(screen_slot)
                            .set_stack(item_stack.clone());
                        player_screen_handler.set_received_stack(screen_slot, item_stack);
                    }
                    if !packet.actions.is_empty() {
                        player_screen_handler.send_content_updates();
                        inventory_updated = true;
                    }
                }
            } else if let Some(plan) =
                validate_legacy_container_actions(Some(player.as_ref()), &packet.actions, &states)
            {
                for (screen_slot, item_stack) in plan.updates {
                    player_screen_handler
                        .get_slot(screen_slot)
                        .set_stack(item_stack.clone());
                    player_screen_handler.set_received_stack(screen_slot, item_stack);
                }
                for dropped in plan.drops {
                    player.drop_item(dropped);
                }
                player_screen_handler.send_content_updates();
                inventory_updated = !packet.actions.is_empty();
            } else {
                rejected_action = true;
            }
            if rejected_action {
                player_screen_handler.send_content_updates();
            }
            drop(player_screen_handler);

            if rejected_action {
                result = 1;
                inventory_updated = true;
            }
        }

        if inventory_updated {
            let slots = player
                .inventory()
                .main_inventory
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .map(NetworkItemStackDescriptor::from)
                .collect();
            self.try_enqueue_client_packet(&CInventoryContent {
                container_id: VarUInt(0),
                slots,
                full_container_name: FullContainerName {
                    container_name: ContainerName::Inventory,
                    dynamic_id: None,
                },
                storage_item: NetworkItemStackDescriptor::default(),
            });
        }

        if rejected_action {
            resync_current_screen_handler(player);
            if packet.legacy_request_id.0 != 0 {
                self.try_enqueue_client_packet(
                    &pumpkin_protocol::bedrock::client::item_stack_response::CItemStackResponse {
                        responses: vec![
                            pumpkin_protocol::bedrock::client::item_stack_response::ItemStackResponseInfo {
                                result,
                                client_request_id: packet.legacy_request_id,
                                containers: Vec::new(),
                            },
                        ],
                    },
                );
            }
            return;
        }

        match packet.transaction_data {
            TransactionData::Normal(_data) => {
                // Actions are already applied to the inventory screen handler above.
            }
            TransactionData::Mismatch(_data) => {
                // Actions are already applied to the inventory screen handler above.
            }
            TransactionData::UseItem(data) => {
                let face = match data.block_face {
                    0 => BlockDirection::Down,
                    2 => BlockDirection::North,
                    3 => BlockDirection::South,
                    4 => BlockDirection::West,
                    5 => BlockDirection::East,
                    _ => BlockDirection::Up,
                };
                let world = player.world();
                let block = world.get_block(&data.block_position);
                let Some(server) = world.server.upgrade() else {
                    return;
                };

                if player.gamemode.load() == GameMode::Spectator {
                    if let Some(factory) = server.block_registry.get_screen_handler_factory(
                        block,
                        player,
                        &data.block_position,
                        &server,
                        &world,
                    ) {
                        player.open_handled_screen(factory.as_ref(), Some(data.block_position));
                    }
                    return;
                }

                if data.action_type.0 == 0 {
                    // Click block
                    let mut held_item = player.inventory().held_item();

                    let result = server.block_registry.use_with_item(
                        block,
                        player,
                        &data.block_position,
                        &BlockHitResult {
                            face: &face,
                            cursor_pos: &data.click_position,
                        },
                        &mut held_item,
                        &EquipmentSlot::MAIN_HAND,
                        &server,
                        &world,
                    );

                    if result.consumes_action() {
                        commit_held_item(player, held_item);
                        return;
                    }

                    if matches!(result, BlockActionResult::PassToDefaultBlockAction) {
                        let result = server.block_registry.on_use(
                            block,
                            player,
                            &data.block_position,
                            &BlockHitResult {
                                face: &face,
                                cursor_pos: &data.click_position,
                            },
                            &server,
                            &world,
                        );

                        if result.consumes_action() {
                            commit_held_item(player, held_item);
                            return;
                        }
                    }

                    let mut stack = held_item;
                    if !stack.is_empty() {
                        let item_id = stack.item.id;
                        let before = stack.clone();
                        player.increment_stat(
                            pumpkin_data::statistic::StatisticCategory::Used,
                            item_id as i32,
                            1,
                        );
                        let item_result = server.item_registry.use_on_block(
                            &mut stack,
                            player,
                            data.block_position,
                            face,
                            data.click_position,
                            block,
                            &server,
                        );

                        if should_try_block_placement(&item_result) {
                            let item_id = stack.item.id;
                            if let Some(placed_block) = pumpkin_data::Block::from_item_id(item_id) {
                                let dummy_use_item_on =
                                    pumpkin_protocol::java::server::play::SUseItemOn {
                                        hand: VarInt(0),
                                        position: data.block_position,
                                        face: VarInt(i32::from(data.block_face)),
                                        cursor_pos: data.click_position,
                                        inside_block: false,
                                        is_against_world_border: false,
                                        sequence: VarInt(0),
                                    };

                                if let Ok(Some(_)) = server.block_registry.place_block(
                                    player,
                                    placed_block,
                                    &server,
                                    &dummy_use_item_on,
                                    data.block_position,
                                    face,
                                ) && player.gamemode.load() != GameMode::Creative
                                {
                                    stack.decrement(1);
                                }
                            }
                        }
                        if before.is_damageable() && stack.is_empty() {
                            player.increment_stat(
                                pumpkin_data::statistic::StatisticCategory::Broken,
                                item_id as i32,
                                1,
                            );
                            player.world().send_entity_status(
                                player.get_entity(),
                                crate::entity::equipment_break_status(&EquipmentSlot::MAIN_HAND),
                                None,
                            );
                        }
                        player.inventory().set_held_item(stack);
                    }
                } else if data.action_type.0 == 1 {
                    // Click air / Use item
                    let mut held = player.inventory.held_item();
                    if !held.is_empty() {
                        player.increment_stat(
                            pumpkin_data::statistic::StatisticCategory::Used,
                            held.item.id as i32,
                            1,
                        );
                    }

                    let event = PlayerInteractEvent::new(
                        player,
                        InteractAction::RightClickAir,
                        &pumpkin_data::Block::AIR,
                        None,
                    );

                    let stack_for_use = held.clone();

                    {
                        let mut cooldown_active = false;
                        if let Some(cooldown) = held.get_use_cooldown() {
                            let group = cooldown
                                .cooldown_group
                                .clone()
                                .unwrap_or_else(|| held.item.registry_key.to_string());
                            if player.is_on_cooldown(&group) {
                                cooldown_active = true;
                            }
                        }

                        if !cooldown_active {
                            // Bedrock can repeat click-air while using an item. Do not
                            // restart its server-side use timer on these timed inputs.
                            let already_using =
                                player.living_entity.item_use_time.load(Ordering::Relaxed) > 0
                                    && player
                                        .living_entity
                                        .item_in_use
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .as_ref()
                                        .is_some_and(|item| {
                                            item.are_items_and_components_equal(&held)
                                        });
                            if !already_using
                                && (held.get_data_component::<ConsumableImpl>().is_some()
                                    || held.get_data_component::<BlocksAttacksImpl>().is_some())
                            {
                                if held
                                    .get_data_component::<FoodImpl>()
                                    .is_none_or(|food| player.can_eat(food.can_always_eat))
                                {
                                    player.living_entity.set_active_hand(
                                        Hand::Right,
                                        held.clone(),
                                        held.get_max_use_time(),
                                    );
                                } else {
                                    // Correct predicted eating when the server's food
                                    // level is already full, even if it has not changed.
                                    self.correct_rejected_food_use(player);
                                }
                            }
                            if let Some(equippable) = held.get_data_component::<EquippableImpl>() {
                                let should_change = {
                                    let inventory = player.inventory();
                                    let equipment_guard = inventory
                                        .entity_equipment
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                                    let current_equipped = equipment_guard.get(equippable.slot);
                                    !current_equipped.are_items_and_components_equal(&held)
                                };
                                if should_change {
                                    player.enqueue_equipment_change(equippable.slot, &held);

                                    let inventory = player.inventory();
                                    let mut equipment_guard = inventory
                                        .entity_equipment
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                                    let equip_item = equipment_guard
                                        .equipment
                                        .entry(equippable.slot.clone())
                                        .or_insert_with(|| ItemStack::EMPTY.clone());
                                    if equip_item.is_empty() {
                                        *equip_item = held.clone();
                                        held.decrement_unless_creative(player.gamemode.load(), 1);
                                    } else {
                                        let old_held = held.clone();
                                        held = equip_item.clone();
                                        *equip_item = old_held;
                                    }
                                    drop(equipment_guard);
                                    player.inventory().set_held_item(held.clone());
                                }
                            }
                        }
                    }

                    send_cancellable_blocking! {{
                        &server;
                        event;
                        'after: {
                            server.item_registry.on_use(&stack_for_use, player);
                        }
                    }}
                }
            }
            TransactionData::UseItemOnEntity(data) => {
                let action = match data.action_type.0 {
                    // Bedrock does not distinguish an entity hit position here. ItemInteract is
                    // therefore exposed as the general Interact action rather than InteractAt.
                    0 | 2 => ActionType::Interact,
                    1 => ActionType::Attack,
                    action => {
                        tracing::warn!("invalid UseItemOnEntity action type {action}");
                        return;
                    }
                };
                let Ok(target_runtime_id) = i32::try_from(data.target_entity_runtime_id.0) else {
                    tracing::warn!(
                        "invalid UseItemOnEntity target runtime ID {}",
                        data.target_entity_runtime_id.0
                    );
                    return;
                };

                let world = player.world();
                let Some(target) = world.get_entity_by_id(target_runtime_id) else {
                    return;
                };
                let Some(server) = world.server.upgrade() else {
                    return;
                };

                let mut event = PlayerInteractEntityEvent::new(
                    player,
                    target.clone(),
                    action,
                    None,
                    player.get_entity().is_sneaking(),
                );
                server.plugin_manager.fire_blocking(&server, &mut event);
                if event.cancelled {
                    return;
                }

                match event.action {
                    ActionType::Interact | ActionType::InteractAt => {
                        let mut stack = player.inventory().held_item();
                        let item_id = stack.item.id;
                        let before = stack.clone();
                        if !event.target.interact(player, &mut stack) {
                            server
                                .item_registry
                                .use_on_entity(&mut stack, player, event.target);
                        }
                        if !stack.are_equal(&before) {
                            player.increment_stat(
                                pumpkin_data::statistic::StatisticCategory::Used,
                                item_id as i32,
                                1,
                            );
                            if before.is_damageable() && stack.is_empty() {
                                player.increment_stat(
                                    pumpkin_data::statistic::StatisticCategory::Broken,
                                    item_id as i32,
                                    1,
                                );
                                player.world().send_entity_status(
                                    player.get_entity(),
                                    crate::entity::equipment_break_status(
                                        &EquipmentSlot::MAIN_HAND,
                                    ),
                                    None,
                                );
                            }
                        }
                        player.inventory().set_held_item(stack);
                    }
                    ActionType::Attack => player.attack(&event.target),
                }
            }
            TransactionData::ReleaseItem(_data) => {
                let Some(server) = player.world().server.upgrade() else {
                    return;
                };
                player.living_entity.stop_using_item(&server, player);
            }
        }

        if packet.legacy_request_id.0 != 0 {
            use pumpkin_protocol::bedrock::client::item_stack_response::{
                CItemStackResponse, ItemStackResponseContainerInfo, ItemStackResponseInfo,
                ItemStackResponseSlotInfo,
            };

            let mut container_infos = Vec::new();
            if result == 0 {
                for update in updates {
                    let container_info = container_infos.iter_mut().find(
                        |info: &&mut ItemStackResponseContainerInfo| {
                            info.full_container_name == update.container_name
                        },
                    );

                    let slot_info = ItemStackResponseSlotInfo {
                        requested_slot: update.slot_id,
                        slot: update.slot_id,
                        amount: update.count,
                        item_stack_net_id: update.stack_id,
                        custom_name: String::new(),
                        filtered_custom_name: String::new(),
                        durability_correction: VarInt(0),
                    };

                    if let Some(info) = container_info {
                        info.slots.push(slot_info);
                    } else {
                        container_infos.push(ItemStackResponseContainerInfo {
                            full_container_name: update.container_name.clone(),
                            slots: vec![slot_info],
                        });
                    }
                }
            }

            self.try_enqueue_client_packet(&CItemStackResponse {
                responses: vec![ItemStackResponseInfo {
                    result,
                    client_request_id: packet.legacy_request_id,
                    containers: container_infos,
                }],
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LegacySlotState, map_bedrock_slot_to_screen_handler, validate_legacy_container_actions,
    };
    use pumpkin_data::{
        data_component::DataComponent,
        data_component_impl::{DataComponentImpl, EquipmentSlot, UnbreakableImpl},
        item::Item,
        item_stack::ItemStack,
    };
    use pumpkin_inventory::{
        inventory::SimpleInventory,
        slot::{ArmorSlot, NormalSlot, Slot},
    };
    use pumpkin_protocol::bedrock::{
        network_item::NetworkItemDescriptor, server::inventory_transaction::InventoryAction,
    };
    use std::sync::Arc;

    fn action_in(
        window_id: i32,
        slot: u32,
        old_item: &ItemStack,
        new_item: &ItemStack,
    ) -> InventoryAction {
        InventoryAction {
            source_type: 0,
            window_id: Some(window_id),
            source_flags: None,
            inventory_slot: slot,
            old_item: NetworkItemDescriptor::from(old_item),
            new_item: NetworkItemDescriptor::from(new_item),
        }
    }

    fn action(slot: u32, old_item: &ItemStack, new_item: &ItemStack) -> InventoryAction {
        action_in(0, slot, old_item, new_item)
    }

    fn state(screen_slot: usize, stack: ItemStack, slot: Arc<dyn Slot>) -> LegacySlotState {
        LegacySlotState {
            screen_slot,
            stack,
            slot,
        }
    }

    fn world_drop(stack: &ItemStack) -> InventoryAction {
        InventoryAction {
            source_type: 2,
            window_id: None,
            source_flags: None,
            inventory_slot: 0,
            old_item: NetworkItemDescriptor::from(ItemStack::EMPTY),
            new_item: NetworkItemDescriptor::from(stack),
        }
    }

    #[test]
    fn rejects_forged_replacement_count_increase_and_duplicate_slot() {
        let current = ItemStack::new(1, &Item::DIRT);
        let forged = action(0, &current, &ItemStack::new(1, &Item::DIAMOND));
        assert!(
            validate_legacy_container_actions(
                None,
                std::slice::from_ref(&forged),
                &[state(
                    36,
                    current.clone(),
                    Arc::new(NormalSlot::new(Arc::new(SimpleInventory::new(1)), 0)),
                )],
            )
            .is_none()
        );

        let increase = action(0, &current, &ItemStack::new(2, &Item::DIRT));
        assert!(
            validate_legacy_container_actions(
                None,
                std::slice::from_ref(&increase),
                &[state(
                    36,
                    current.clone(),
                    Arc::new(NormalSlot::new(Arc::new(SimpleInventory::new(1)), 0)),
                )],
            )
            .is_none()
        );

        let duplicate = vec![
            action(0, &current, &ItemStack::EMPTY),
            action(0, &current, &ItemStack::EMPTY),
        ];
        assert!(
            validate_legacy_container_actions(
                None,
                &duplicate,
                &[state(
                    36,
                    current,
                    Arc::new(NormalSlot::new(Arc::new(SimpleInventory::new(1)), 0)),
                )],
            )
            .is_none()
        );
    }

    #[test]
    fn rejects_an_invalid_batch_without_changing_any_snapshot() {
        let dirt = ItemStack::new(1, &Item::DIRT);
        let stone = ItemStack::new(1, &Item::STONE);
        let states = vec![
            state(
                36,
                dirt.clone(),
                Arc::new(NormalSlot::new(Arc::new(SimpleInventory::new(1)), 0)),
            ),
            state(
                37,
                stone.clone(),
                Arc::new(NormalSlot::new(Arc::new(SimpleInventory::new(1)), 0)),
            ),
        ];
        let actions = vec![
            action(0, &dirt, &stone),
            action(1, &ItemStack::new(1, &Item::DIAMOND), &dirt),
        ];
        assert!(validate_legacy_container_actions(None, &actions, &states).is_none());
        assert!(states[0].stack.are_equal(&dirt));
        assert!(states[1].stack.are_equal(&stone));
    }

    #[test]
    fn validates_world_drop_with_atomic_removal_plan() {
        let dirt = ItemStack::new(1, &Item::DIRT);
        let actions = vec![action(0, &dirt, &ItemStack::EMPTY), world_drop(&dirt)];
        let plan = validate_legacy_container_actions(
            None,
            &actions,
            &[state(
                36,
                dirt.clone(),
                Arc::new(NormalSlot::new(Arc::new(SimpleInventory::new(1)), 0)),
            )],
        )
        .expect("conserved removal plus drop");
        assert_eq!(plan.updates.len(), 1);
        assert!(plan.updates[0].1.is_empty());
        assert_eq!(plan.drops.len(), 1);
        assert!(plan.drops[0].are_equal(&dirt));

        let forged = vec![
            action(0, &dirt, &ItemStack::EMPTY),
            world_drop(&ItemStack::new(2, &Item::DIRT)),
        ];
        assert!(
            validate_legacy_container_actions(
                None,
                &forged,
                &[state(
                    36,
                    dirt,
                    Arc::new(NormalSlot::new(Arc::new(SimpleInventory::new(1)), 0)),
                )],
            )
            .is_none()
        );
    }

    #[test]
    fn rejects_unsupported_authoritative_components() {
        let mut customized = ItemStack::new(1, &Item::STONE);
        customized
            .patch
            .push((DataComponent::Unbreakable, Some(UnbreakableImpl.to_dyn())));
        let dirt = ItemStack::new(1, &Item::DIRT);
        let actions = vec![action(0, &customized, &dirt), action(1, &dirt, &customized)];
        let states = vec![
            state(
                36,
                customized,
                Arc::new(NormalSlot::new(Arc::new(SimpleInventory::new(1)), 0)),
            ),
            state(
                37,
                dirt,
                Arc::new(NormalSlot::new(Arc::new(SimpleInventory::new(1)), 0)),
            ),
        ];
        assert!(validate_legacy_container_actions(None, &actions, &states).is_none());
    }

    #[test]
    fn unsupported_ui_window_is_fail_closed() {
        assert_eq!(map_bedrock_slot_to_screen_handler(124, 0), None);
    }

    #[test]
    fn applies_existing_armor_and_offhand_slot_policy() {
        let inventory: Arc<dyn pumpkin_inventory::Inventory> = Arc::new(SimpleInventory::new(1));
        let normal_slot: Arc<dyn Slot> = Arc::new(NormalSlot::new(inventory.clone(), 0));
        let armor_slot: Arc<dyn Slot> =
            Arc::new(ArmorSlot::new(inventory.clone(), 0, EquipmentSlot::HEAD));
        let empty = ItemStack::EMPTY.clone();
        let dirt = ItemStack::new(1, &Item::DIRT);
        let helmet = ItemStack::new(1, &Item::IRON_HELMET);

        let reject_dirt = vec![
            action_in(0, 0, &dirt, &empty),
            action_in(120, 0, &empty, &dirt),
        ];
        assert!(
            validate_legacy_container_actions(
                None,
                &reject_dirt,
                &[
                    state(36, dirt.clone(), normal_slot.clone()),
                    state(5, empty.clone(), armor_slot.clone()),
                ],
            )
            .is_none()
        );

        let accept_helmet = vec![
            action_in(0, 0, &helmet, &empty),
            action_in(120, 0, &empty, &helmet),
        ];
        assert!(
            validate_legacy_container_actions(
                None,
                &accept_helmet,
                &[
                    state(36, helmet, normal_slot.clone()),
                    state(5, empty.clone(), armor_slot),
                ],
            )
            .is_some()
        );

        let offhand = Arc::new(NormalSlot::new(inventory, 0)) as Arc<dyn Slot>;
        let offhand_actions = vec![
            action_in(0, 0, &dirt, &ItemStack::EMPTY),
            action_in(119, 0, &ItemStack::EMPTY, &dirt),
        ];
        assert!(
            validate_legacy_container_actions(
                None,
                &offhand_actions,
                &[
                    state(36, dirt, normal_slot),
                    state(45, ItemStack::EMPTY.clone(), offhand),
                ],
            )
            .is_some()
        );
    }
}
