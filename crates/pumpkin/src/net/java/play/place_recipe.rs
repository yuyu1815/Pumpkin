#[allow(clippy::wildcard_imports)]
use super::*;

impl JavaClient {
    #[allow(clippy::too_many_lines)]
    pub fn handle_place_recipe(
        &self,
        server: &Arc<Server>,
        player: &Arc<Player>,
        packet: &SPlaceRecipe,
    ) {
        use crate::net::java::recipe_helper::{
            GenericIngredient, compute_biggest_craftable, take_n_ingredient,
        };
        use crate::server::recipe::DynamicRecipe;
        use pumpkin_data::recipes::{CraftingRecipeTypes, RECIPES_COOKING, RECIPES_CRAFTING};
        use pumpkin_data::screen::WindowType;
        use pumpkin_inventory::crafting::recipe_provider::RecipeProvider;
        use pumpkin_protocol::java::client::play::{
            CPlaceGhostRecipe, RecipeDisplay, crafting_recipe_display,
            dynamic_recipe_for_display_id,
        };

        let target_id = packet.recipe_display_id.0 as usize;
        let use_max = packet.use_max_items;
        let current_handler = player
            .current_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let current_window_id = current_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sync_id();
        if !placement_container_matches(packet.container_id, current_window_id) {
            return;
        }

        let mut click_event = crate::plugin::api::events::player::player_recipe_book_click::PlayerRecipeBookClickEvent::new(
            player.clone(),
            format!("display_{}", packet.recipe_display_id.0),
            use_max,
        );
        server
            .plugin_manager
            .fire_blocking(server, &mut click_event);
        if click_event.cancelled {
            return;
        }

        // Count crafting display IDs.
        let crafting_display_count = RECIPES_CRAFTING
            .iter()
            .filter(|r| {
                !matches!(
                    r,
                    CraftingRecipeTypes::CraftingSpecial
                        | CraftingRecipeTypes::CraftingDecoratedPot { .. }
                )
            })
            .count();
        let cooking_display_count = RECIPES_COOKING.len();
        let dynamic_recipes = server.recipe_manager.get_dynamic_recipes();

        let (grid_width, crafting_inv) = {
            let handler = current_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let grid_width: usize = match handler.window_type() {
                Some(WindowType::Crafting) => 3,
                None => 2, // player inventory 2x2
                _ => return,
            };
            (grid_width, handler.get_behaviour().slots[1].get_inventory())
        };

        let grid_size = grid_width * grid_width;
        let mut ingredient_slots: Vec<Option<GenericIngredient<'_>>> = vec![None; grid_size];
        let mut ghost_display: Option<RecipeDisplay<'static>> = None;

        if target_id < crafting_display_count {
            // Crafting recipe
            let mut counter = 0usize;
            let recipe = RECIPES_CRAFTING.iter().find(|r| {
                if matches!(
                    r,
                    CraftingRecipeTypes::CraftingSpecial
                        | CraftingRecipeTypes::CraftingDecoratedPot { .. }
                ) {
                    return false;
                }
                let found = counter == target_id;
                counter += 1;
                found
            });
            let Some(recipe) = recipe else { return };
            if self.version.load() >= pumpkin_util::version::JavaMinecraftVersion::V_26_2 {
                ghost_display = crafting_recipe_display(recipe, self.version.load());
            }

            match recipe {
                CraftingRecipeTypes::CraftingShaped { pattern, key, .. } => {
                    if !shaped_pattern_fits(pattern, grid_width) {
                        return;
                    }
                    for (row, row_str) in pattern.iter().enumerate() {
                        for (col, ch) in row_str.chars().enumerate() {
                            if ch != ' '
                                && let Some(ing) =
                                    key.iter().find_map(|(k, v)| (*k == ch).then_some(v))
                                && col < grid_width
                                && row * grid_width + col < grid_size
                            {
                                ingredient_slots[row * grid_width + col] =
                                    Some(GenericIngredient::Vanilla(ing));
                            }
                        }
                    }
                }
                CraftingRecipeTypes::CraftingShapeless { ingredients, .. } => {
                    for (i, ing) in ingredients.iter().enumerate().take(grid_size) {
                        ingredient_slots[i] = Some(GenericIngredient::Vanilla(ing));
                    }
                }
                CraftingRecipeTypes::CraftingTransmute {
                    input, material, ..
                } => {
                    if grid_size >= 2 {
                        ingredient_slots[0] = Some(GenericIngredient::Vanilla(input));
                        ingredient_slots[1] = Some(GenericIngredient::Vanilla(material));
                    }
                }
                _ => return,
            }
        } else if target_id < crafting_display_count + cooking_display_count {
            // TODO: cooking recipes
            return;
        } else {
            let dynamic_id = target_id - crafting_display_count - cooking_display_count;
            let Some(DynamicRecipe::Crafting(crafting)) =
                dynamic_recipe_for_display_id(&dynamic_recipes, dynamic_id)
            else {
                return;
            };

            match crafting {
                pumpkin_protocol::codec::recipe::OwnedCraftingRecipe::Shaped {
                    pattern,
                    key,
                    ..
                } => {
                    if !shaped_pattern_fits(pattern, grid_width) {
                        return;
                    }
                    for (row, row_str) in pattern.iter().enumerate() {
                        for (col, ch) in row_str.chars().enumerate() {
                            if ch != ' '
                                && let Some((_, ing)) = key.iter().find(|(k, _)| *k == ch)
                                && col < grid_width
                                && row * grid_width + col < grid_size
                            {
                                ingredient_slots[row * grid_width + col] =
                                    Some(GenericIngredient::Dynamic(ing));
                            }
                        }
                    }
                }

                pumpkin_protocol::codec::recipe::OwnedCraftingRecipe::Shapeless {
                    ingredients,
                    ..
                } => {
                    for (i, ing) in ingredients.iter().enumerate().take(grid_size) {
                        ingredient_slots[i] = Some(GenericIngredient::Dynamic(ing));
                    }
                }
            }
        }

        // Check if this exact recipe is already placed (determines stacking vs fresh fill).
        let recipe_matches = {
            let mut ok = true;
            for (idx, ing) in ingredient_slots.iter().enumerate() {
                let stack = crafting_inv.get_stack(idx);
                match ing {
                    None => {
                        if !stack.is_empty() {
                            ok = false;
                            break;
                        }
                    }
                    Some(ingredient) => {
                        if stack.is_empty() || !ingredient.match_item(stack.item) {
                            ok = false;
                            break;
                        }
                    }
                }
            }
            ok
        };

        // Read minimum count from occupied slots before clearing (needed for stacking).
        let current_min = if recipe_matches {
            let mut min = u8::MAX;
            for (idx, ing) in ingredient_slots.iter().enumerate() {
                if ing.is_some() {
                    let stack = crafting_inv.get_stack(idx);
                    if !stack.is_empty() {
                        min = min.min(stack.item_count);
                    }
                }
            }
            if min == u8::MAX { 0 } else { min }
        } else {
            0
        };

        if recipe_matches {
            let next_amount = current_min.saturating_add(1);
            let exceeds_current_capacity = ingredient_slots.iter().enumerate().any(|(idx, ing)| {
                ing.is_some() && next_amount > crafting_inv.get_stack(idx).get_max_stack_size()
            });
            if exceeds_current_capacity {
                let screen_handler_arc = player
                    .current_screen_handler
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                screen_handler_arc
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .send_content_updates();
                return;
            }
        }

        // Always clear the grid first, returning items to inventory.
        for i in 0..grid_size {
            let stack = crafting_inv.remove_stack(i);
            if !stack.is_empty() {
                player.inventory.offer(stack, false, player.as_ref());
            }
        }

        // Determine how many of each ingredient to place per slot.
        let active_ingredients: Vec<GenericIngredient<'_>> =
            ingredient_slots.iter().flatten().copied().collect();
        let amount_to_craft = if use_max {
            compute_biggest_craftable(&active_ingredients, &player.inventory)
        } else if recipe_matches {
            current_min.saturating_add(1)
        } else {
            1
        };

        if amount_to_craft == 0 {
            if should_send_ghost(self.version.load(), packet.container_id, current_window_id)
                && let Some(display) = ghost_display.as_ref()
            {
                self.try_send_packet(&CPlaceGhostRecipe::with_display(current_window_id, display));
            }
            current_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .send_content_updates();
            return;
        }

        // Confirm the same first-fit allocation used by the donor helper before
        // consuming anything; otherwise a tag/one-of choice can consume the
        // donor needed by a later ingredient.
        if compute_biggest_craftable(&active_ingredients, &player.inventory) < amount_to_craft {
            if should_send_ghost(self.version.load(), packet.container_id, current_window_id)
                && let Some(display) = ghost_display.as_ref()
            {
                self.try_send_packet(&CPlaceGhostRecipe::with_display(current_window_id, display));
            }
            let screen_handler_arc = player
                .current_screen_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            screen_handler_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .send_content_updates();
            return;
        }

        // Fill each grid slot with exactly `amount_to_craft` matching items.
        for (idx, ing) in ingredient_slots.iter().enumerate() {
            let Some(ingredient) = ing else { continue };
            let taken = take_n_ingredient(&player.inventory, ingredient, amount_to_craft);
            if taken.item_count == amount_to_craft {
                crafting_inv.set_stack(idx, taken);
            }
        }

        let screen_handler_arc = player
            .current_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        screen_handler_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send_content_updates();
    }
}

fn placement_container_matches(packet_id: i8, current_id: u8) -> bool {
    packet_id >= 0 && i32::from(packet_id) == i32::from(current_id)
}

fn shaped_pattern_fits<S: AsRef<str>>(pattern: &[S], grid_width: usize) -> bool {
    pattern.len() <= grid_width
        && pattern
            .iter()
            .all(|row| row.as_ref().chars().count() <= grid_width)
}

fn should_send_ghost(
    version: pumpkin_util::version::JavaMinecraftVersion,
    packet_id: i8,
    current_id: u8,
) -> bool {
    version >= pumpkin_util::version::JavaMinecraftVersion::V_26_2
        && placement_container_matches(packet_id, current_id)
}

#[cfg(test)]
mod tests {
    use super::{placement_container_matches, shaped_pattern_fits, should_send_ghost};
    use pumpkin_util::version::JavaMinecraftVersion;

    #[test]
    fn ghost_requires_matching_container_and_26_2() {
        assert!(should_send_ghost(JavaMinecraftVersion::V_26_2, 7, 7));
    }

    #[test]
    fn mismatched_container_never_gets_a_ghost() {
        assert!(!placement_container_matches(6, 7));
        assert!(!placement_container_matches(-1, 7));
        assert!(!should_send_ghost(JavaMinecraftVersion::V_26_2, 6, 7));
        assert!(!should_send_ghost(JavaMinecraftVersion::V_26_1, 7, 7));
    }

    #[test]
    fn shaped_patterns_must_fit_each_grid_dimension() {
        assert!(shaped_pattern_fits(&["###", " # "], 3));
        assert!(!shaped_pattern_fits(&["###"], 2));
        assert!(!shaped_pattern_fits(&["#", "#", "#"], 2));
    }
}
