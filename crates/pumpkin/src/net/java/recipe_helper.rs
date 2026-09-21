use pumpkin_data::item::Item;
use pumpkin_data::item_stack::ItemStack;
use pumpkin_data::recipes::RecipeIngredientTypes;
use pumpkin_inventory::player::player_inventory::PlayerInventory;
use pumpkin_protocol::codec::recipe::OwnedRecipeIngredient;

#[derive(Clone, Copy)]
pub enum GenericIngredient<'a> {
    Vanilla(&'a RecipeIngredientTypes),
    Dynamic(&'a OwnedRecipeIngredient),
}

fn is_usable_for_crafting(stack: &ItemStack) -> bool {
    !stack.is_empty()
        && stack.get_damage() == 0
        && !stack.has_enchantments()
        && !stack.has_custom_name()
}

impl GenericIngredient<'_> {
    #[must_use]
    pub fn match_item(&self, item: &Item) -> bool {
        match self {
            Self::Vanilla(v) => v.match_item(item),
            Self::Dynamic(d) => d.match_item(item),
        }
    }
}

pub fn take_n_ingredient(
    inventory: &PlayerInventory,
    ingredient: &GenericIngredient<'_>,
    count: u8,
) -> ItemStack {
    if count == 0 {
        return ItemStack::EMPTY.clone();
    }

    let mut taken = 0u8;
    let mut result: Option<ItemStack> = None;

    let mut main_inventory = inventory
        .main_inventory
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let primary = main_inventory
        .iter()
        .enumerate()
        .find(|(_, stack)| {
            is_usable_for_crafting(stack)
                && ingredient.match_item(stack.item)
                && stack.item_count.min(stack.get_max_stack_size()) >= count
        })
        .or_else(|| {
            main_inventory.iter().enumerate().find(|(_, stack)| {
                is_usable_for_crafting(stack) && ingredient.match_item(stack.item)
            })
        })
        .map(|(index, _)| index);
    let Some(primary) = primary else {
        return ItemStack::EMPTY.clone();
    };

    for index in
        std::iter::once(primary).chain((0..main_inventory.len()).filter(|&index| index != primary))
    {
        let stack = &mut main_inventory[index];
        if !is_usable_for_crafting(stack) || !ingredient.match_item(stack.item) {
            continue;
        }

        let available = stack.item_count.min(stack.get_max_stack_size());
        let room = result.as_ref().map_or(count, |r| {
            r.get_max_stack_size().saturating_sub(r.item_count)
        });
        let to_take = count.saturating_sub(taken).min(available).min(room);
        if to_take == 0 {
            continue;
        }

        if result
            .as_ref()
            .is_some_and(|r| !r.are_items_and_components_equal(stack))
        {
            continue;
        }

        let sub_stack = stack.split(to_take);
        taken = taken.saturating_add(sub_stack.item_count);
        match &mut result {
            None => result = Some(sub_stack),
            Some(r) => r.item_count = r.item_count.saturating_add(sub_stack.item_count),
        }

        if taken >= count {
            break;
        }
    }
    result.unwrap_or_else(|| ItemStack::EMPTY.clone())
}

pub fn compute_biggest_craftable(
    ingredients: &[GenericIngredient<'_>],
    inventory: &PlayerInventory,
) -> u8 {
    let mut available = Vec::new();
    let main_inventory = inventory
        .main_inventory
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for stack in main_inventory.iter() {
        if is_usable_for_crafting(stack) {
            available.push(stack.copy_with_count(stack.item_count.min(stack.get_max_stack_size())));
        }
    }

    let max_amount = available
        .iter()
        .map(ItemStack::get_max_stack_size)
        .max()
        .unwrap_or(0);
    'outer: for amount in (1..=max_amount).rev() {
        let mut budget = available.clone();
        for ing in ingredients {
            let Some(stack) = budget.iter_mut().find(|stack| {
                stack.item_count >= amount
                    && stack.get_max_stack_size() >= amount
                    && ing.match_item(stack.item)
            }) else {
                continue 'outer;
            };
            stack.item_count -= amount;
        }
        return amount;
    }
    0
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use pumpkin_data::data_component_impl::{EquipmentSlot, MaxStackSizeImpl};
    use pumpkin_data::item::Item;
    use pumpkin_data::item_stack::ItemStack;
    use pumpkin_data::recipes::RecipeIngredientTypes;
    use pumpkin_inventory::Inventory;
    use pumpkin_inventory::crafting::crafting_inventory::CraftingInventory;
    use pumpkin_inventory::entity_equipment::EntityEquipment;
    use rustc_hash::FxHashMap;

    use super::{GenericIngredient, compute_biggest_craftable, take_n_ingredient};

    fn make_inventory() -> pumpkin_inventory::player::player_inventory::PlayerInventory {
        pumpkin_inventory::player::player_inventory::PlayerInventory::new(
            Arc::new(Mutex::new(EntityEquipment::new())),
            Arc::new(FxHashMap::<usize, EquipmentSlot>::default()),
        )
    }

    #[test]
    fn use_max_respects_effective_stack_size() {
        let bow_ingredient = RecipeIngredientTypes::Simple("minecraft:bow");
        let ingredient = GenericIngredient::Vanilla(&bow_ingredient);
        let inventory = make_inventory();
        {
            let mut slots = inventory.main_inventory.write().unwrap();
            slots[0] = ItemStack::new(1, &Item::BOW);
            slots[1] = ItemStack::new(1, &Item::BOW);
        }
        assert_eq!(compute_biggest_craftable(&[ingredient], &inventory), 1);
        assert_eq!(
            compute_biggest_craftable(&[ingredient, ingredient], &inventory),
            1
        );

        let dirt_ingredient = RecipeIngredientTypes::Simple("minecraft:dirt");
        let ingredient = GenericIngredient::Vanilla(&dirt_ingredient);
        let inventory = make_inventory();
        inventory.main_inventory.write().unwrap()[0] = ItemStack::new(64, &Item::DIRT);
        assert_eq!(compute_biggest_craftable(&[ingredient], &inventory), 64);

        let pearl_ingredient = RecipeIngredientTypes::Simple("minecraft:ender_pearl");
        let ingredient = GenericIngredient::Vanilla(&pearl_ingredient);
        let inventory = make_inventory();
        {
            let mut slots = inventory.main_inventory.write().unwrap();
            slots[0] = ItemStack::new(16, &Item::ENDER_PEARL);
            slots[1] = ItemStack::new(16, &Item::ENDER_PEARL);
        }
        assert_eq!(compute_biggest_craftable(&[ingredient], &inventory), 16);

        let coal_ingredient = RecipeIngredientTypes::Simple("minecraft:coal");
        let ingredient = GenericIngredient::Vanilla(&coal_ingredient);
        let inventory = make_inventory();
        {
            let mut slots = inventory.main_inventory.write().unwrap();
            for slot in &mut slots[..2] {
                let mut stack = ItemStack::new(16, &Item::COAL);
                stack.set_data_component(MaxStackSizeImpl { size: 16 });
                *slot = stack;
            }
        }
        assert_eq!(compute_biggest_craftable(&[ingredient], &inventory), 16);
    }

    #[test]
    fn take_n_ingredient_respects_grid_stack_capacity() {
        let inventory = make_inventory();
        {
            let mut slots = inventory.main_inventory.write().unwrap();
            for slot in &mut slots[..2] {
                let mut stack = ItemStack::new(16, &Item::COAL);
                stack.set_data_component(MaxStackSizeImpl { size: 16 });
                *slot = stack;
            }
        }

        let coal_ingredient = RecipeIngredientTypes::Simple("minecraft:coal");
        let taken = take_n_ingredient(
            &inventory,
            &GenericIngredient::Vanilla(&coal_ingredient),
            32,
        );
        let grid = CraftingInventory::new(2, 2);
        grid.set_stack(0, taken);
        assert_eq!(grid.get_stack(0).item_count, 16);
        assert_eq!(grid.get_stack(0).get_max_stack_size(), 16);
    }

    #[test]
    fn take_uses_the_same_first_matching_donor_as_compute() {
        let inventory = make_inventory();
        {
            let mut slots = inventory.main_inventory.write().unwrap();
            slots[0] = ItemStack::new(2, &Item::BIRCH_PLANKS);
            slots[1] = ItemStack::new(3, &Item::OAK_PLANKS);
        }

        let choices =
            RecipeIngredientTypes::OneOf(&["minecraft:birch_planks", "minecraft:oak_planks"]);
        let oak = RecipeIngredientTypes::Simple("minecraft:oak_planks");
        let ingredients = [
            GenericIngredient::Vanilla(&choices),
            GenericIngredient::Vanilla(&oak),
        ];
        assert_eq!(compute_biggest_craftable(&ingredients, &inventory), 2);
        let first = take_n_ingredient(&inventory, &ingredients[0], 2);
        let second = take_n_ingredient(&inventory, &ingredients[1], 2);
        assert_eq!(first.item, &Item::BIRCH_PLANKS);
        assert_eq!(first.item_count, 2);
        assert_eq!(second.item, &Item::OAK_PLANKS);
        assert_eq!(second.item_count, 2);
    }

    #[test]
    fn take_n_ingredient_does_not_merge_different_components() {
        let inventory = make_inventory();
        {
            let mut slots = inventory.main_inventory.write().unwrap();
            let mut first = ItemStack::new(8, &Item::COAL);
            first.set_data_component(MaxStackSizeImpl { size: 16 });
            let mut second = ItemStack::new(8, &Item::COAL);
            second.set_data_component(MaxStackSizeImpl { size: 32 });
            slots[0] = first;
            slots[1] = second;
        }

        let coal_ingredient = RecipeIngredientTypes::Simple("minecraft:coal");
        let taken = take_n_ingredient(
            &inventory,
            &GenericIngredient::Vanilla(&coal_ingredient),
            16,
        );
        assert_eq!(taken.item_count, 8);
        assert_eq!(taken.get_max_stack_size(), 16);
        assert_eq!(inventory.get_slot(1).item_count, 8);
    }
}
