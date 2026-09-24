//! Player inventory screen handler.
//!
//! This module handles the player's inventory screen (opened with E key).
//! It includes:
//! - The 2x2 crafting grid (inventory crafting)
//! - Armor slots (head, chest, legs, feet)
//! - Main inventory (27 slots)
//! - Hotbar (9 slots)
//! - Offhand slot
//!
//! # Slot Layout
//!
//! The player screen handler uses the following slot indices:
//! - 0: Crafting result
//! - 1-4: Crafting grid (2x2)
//! - 5-8: Armor slots (head, chest, legs, feet)
//! - 9-35: Main inventory
//! - 36-44: Hotbar
//! - 45: Offhand

use super::player_inventory::PlayerInventory;
use crate::crafting::crafting_inventory::CraftingInventory;
use crate::crafting::crafting_screen_handler::CraftingScreenHandler;
use crate::crafting::recipes::{RecipeFinderScreenHandler, RecipeInputInventory};
use crate::inventory::Inventory;
use crate::screen_handler::{InventoryPlayer, ScreenHandler, ScreenHandlerBehaviour};
use crate::slot::{ArmorSlot, NormalSlot, Slot};
use pumpkin_data::data_component_impl::{EquipmentSlot, EquipmentType, EquippableImpl};
use pumpkin_data::item_stack::ItemStack;
use pumpkin_data::screen::WindowType;
use std::any::Any;
use std::sync::Arc;

/// Screen handler for the player's inventory.
///
/// Manages the player's inventory UI including crafting, armor, and
/// the main inventory. This is the default screen shown when pressing E.
pub struct PlayerScreenHandler {
    /// Core screen handler behavior (slots, sync ID, listeners).
    behaviour: ScreenHandlerBehaviour,
    /// The 2x2 crafting grid inventory.
    crafting_inventory: Arc<dyn RecipeInputInventory>,
}

impl RecipeFinderScreenHandler for PlayerScreenHandler {}

impl CraftingScreenHandler<CraftingInventory> for PlayerScreenHandler {}

// TODO: Fully implement this
impl PlayerScreenHandler {
    /// Equipment slot order for armor display.
    const EQUIPMENT_SLOT_ORDER: [EquipmentSlot; 4] = [
        EquipmentSlot::HEAD,
        EquipmentSlot::CHEST,
        EquipmentSlot::LEGS,
        EquipmentSlot::FEET,
    ];

    /// Checks if a slot index is in the hotbar.
    ///
    /// Hotbar slots are 36-44 in the protocol (0-indexed 36-44).
    #[must_use]
    pub fn is_in_hotbar(slot: u8) -> bool {
        (36..=45).contains(&slot)
    }

    /// Gets a slot by its index.
    pub fn get_slot(&self, slot: usize) -> Arc<dyn Slot> {
        self.behaviour.slots[slot].clone()
    }

    /// Creates a new player screen handler.
    ///
    /// # Arguments
    /// - `player_inventory` - The player's inventory
    /// - `window_type` - The window type (usually None for player inventory)
    /// - `sync_id` - The synchronization ID
    pub fn new(
        player_inventory: &Arc<PlayerInventory>,
        window_type: Option<WindowType>,
        sync_id: u8,
        provider: Option<Arc<dyn crate::crafting::recipe_provider::RecipeProvider>>,
    ) -> Self {
        let crafting_inventory: Arc<dyn RecipeInputInventory> =
            Arc::new(CraftingInventory::new(2, 2));

        let mut player_screen_handler = Self {
            behaviour: ScreenHandlerBehaviour::new(sync_id, window_type),
            crafting_inventory: crafting_inventory.clone(),
        };

        player_screen_handler.add_recipe_slots(crafting_inventory, provider);

        // Add armor slots (head, chest, legs, feet)
        for i in 0..4 {
            player_screen_handler.add_slot(Arc::new(ArmorSlot::new(
                player_inventory.clone(),
                39 - i,
                Self::EQUIPMENT_SLOT_ORDER[i].clone(),
            )));
        }

        let player_inventory: Arc<dyn Inventory> = player_inventory.clone();

        // Add main inventory and hotbar
        player_screen_handler.add_player_slots(&player_inventory);

        // Offhand slot (index 40 in player inventory, 45 in screen handler)
        // TODO: onEquipStack callback for offhand
        player_screen_handler.add_slot(Arc::new(NormalSlot::new(player_inventory.clone(), 40)));

        player_screen_handler
    }
}

impl ScreenHandler for PlayerScreenHandler {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn get_behaviour(&self) -> &ScreenHandlerBehaviour {
        &self.behaviour
    }

    fn get_behaviour_mut(&mut self) -> &mut ScreenHandlerBehaviour {
        &mut self.behaviour
    }

    fn on_closed(&mut self, player: &dyn InventoryPlayer) {
        self.default_on_closed(player);
        //TODO: this.craftingResultInventory.clear();
        self.drop_inventory(player, self.crafting_inventory.clone());
    }

    /// Performs quick move (shift-click) for the given slot.
    ///
    /// The quick move logic depends on the source slot:
    /// - Crafting result (0) -> Player inventory (from end)
    /// - Crafting grid (1-4) -> Player inventory (from start)
    /// - Armor slots (5-8) -> Player inventory, unequips
    /// - Armor items -> Armor slots if empty
    /// - Offhand items -> Offhand slot if empty
    /// - Main inventory (9-35) -> Hotbar
    /// - Hotbar (36-44) -> Main inventory
    fn quick_move(&mut self, player: &dyn InventoryPlayer, slot_index: i32) -> ItemStack {
        let slot = self.get_behaviour().slots[slot_index as usize].clone();

        // TODO: Equippable component

        if slot.has_stack() {
            let mut slot_stack = slot.get_stack();
            let stack_prev = slot_stack.clone();

            let equipment_slot = slot_stack
                .get_data_component::<EquippableImpl>()
                .map_or(&EquipmentSlot::MAIN_HAND, |equippable| equippable.slot);

            // Quick move logic based on source slot
            let success = if slot_index == 0 {
                // From crafting result slot (0) -> Player Inventory (9-45, from end)
                self.insert_item(&mut slot_stack, 9, 45, true)
            } else if (1..5).contains(&slot_index) {
                // From craft ingredient slots (1-4) -> Player Inventory (9-45, from start)
                self.insert_item(&mut slot_stack, 9, 45, false)
            } else if (5..9).contains(&slot_index) {
                // From armour slots (5-8) -> Player Inventory (9-45, from start)
                let result = self.insert_item(&mut slot_stack, 9, 45, false);

                if result {
                    player.enqueue_equipment_change(equipment_slot, ItemStack::EMPTY);
                }
                result
            } else if equipment_slot.slot_type() == EquipmentType::HumanoidArmor
                && self
                    .get_slot((8 - equipment_slot.get_entity_slot_id()) as usize)
                    .get_cloned_stack()
                    .is_empty()
            {
                // Into empty armour slots (5-8)
                let index = 8 - equipment_slot.get_entity_slot_id();
                let result = self.insert_item(&mut slot_stack, index, index + 1, false);

                if result {
                    player.enqueue_equipment_change(equipment_slot, &stack_prev);
                }
                result
            } else if matches!(equipment_slot, EquipmentSlot::OffHand(_))
                && slot_index != 45
                && self.get_slot(45).get_cloned_stack().is_empty()
            {
                // Into empty offhand slot (45)
                let index = 45;
                self.insert_item(&mut slot_stack, index, index + 1, false)
            } else if (9..36).contains(&slot_index) {
                // From main inventory (9-35) -> Hotbar (36-44)
                self.insert_item(&mut slot_stack, 36, 45, false)
            } else if (36..45).contains(&slot_index) {
                // From hotbar (36-44) -> Main inventory (9-35)
                self.insert_item(&mut slot_stack, 9, 36, false)
            } else {
                // Fallback to moving into the player inventory area
                self.insert_item(&mut slot_stack, 9, 45, false)
            };

            if !success {
                return ItemStack::EMPTY.clone();
            }

            let stack = slot_stack.clone();

            if stack.is_empty() {
                slot.set_stack_prev(ItemStack::EMPTY.clone(), stack_prev.clone());
            } else {
                slot.set_stack(stack.clone());
            }

            if stack.item_count == stack_prev.item_count {
                return ItemStack::EMPTY.clone();
            }

            let mut taken_stack = stack_prev.clone();
            taken_stack.set_count(stack_prev.item_count - stack.item_count);
            slot.on_take_item(player, &taken_stack);

            if slot_index == 0 {
                // From crafting result slot (0)
                // Notify the result slot to refill
                slot.on_quick_move_crafted(stack.clone(), stack_prev.clone());
                // For crafting result slot, drop any remaining items
                if !stack.is_empty() {
                    player.drop_item(stack, false);
                }
            }

            return stack_prev;
        }

        // Nothing changed
        ItemStack::EMPTY.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crafting::crafting_screen_handler::CraftingTableScreenHandler;
    use crate::entity_equipment::EntityEquipment;
    use pumpkin_data::data_component_impl::{EquipmentSlot, MapIdImpl};
    use pumpkin_data::sound::Sound;
    use pumpkin_data::statistic::StatisticCategory;
    use pumpkin_protocol::java::client::play::{
        CSetContainerContent, CSetContainerProperty, CSetContainerSlot, CSetCursorItem,
        CSetPlayerInventory, CSetSelectedSlot,
    };
    use pumpkin_protocol::java::server::play::SlotActionType;
    use std::any::Any;
    use std::sync::{Arc, Mutex};

    struct DropRecorder {
        inventory: Arc<PlayerInventory>,
        drops: Mutex<Vec<ItemStack>>,
    }

    impl DropRecorder {
        fn new(inventory: Arc<PlayerInventory>) -> Self {
            Self {
                inventory,
                drops: Mutex::new(Vec::new()),
            }
        }

        fn dropped_count(&self, item: &'static pumpkin_data::item::Item) -> u32 {
            self.drops
                .lock()
                .unwrap()
                .iter()
                .filter(|stack| stack.item == item)
                .map(|stack| u32::from(stack.item_count))
                .sum()
        }
    }

    impl InventoryPlayer for DropRecorder {
        fn as_any(&self) -> &dyn Any {
            self
        }

        fn drop_item(&self, item: ItemStack, _retain_ownership: bool) {
            self.drops.lock().unwrap().push(item);
        }

        fn get_inventory(&self) -> Arc<PlayerInventory> {
            self.inventory.clone()
        }

        fn has_infinite_materials(&self) -> bool {
            false
        }

        fn is_creative(&self) -> bool {
            false
        }

        fn experience_level(&self) -> i32 {
            0
        }

        fn add_experience_levels(&self, _levels: i32) {}

        fn enchantment_seed(&self) -> i32 {
            0
        }

        fn set_enchantment_seed(&self, _seed: i32) {}

        fn enqueue_inventory_packet(
            &self,
            _packet: &CSetContainerContent,
            _window_type: Option<WindowType>,
        ) {
        }

        fn enqueue_slot_packet(
            &self,
            _packet: &CSetContainerSlot,
            _window_type: Option<WindowType>,
            _total_slots: usize,
        ) {
        }

        fn enqueue_cursor_packet(&self, _packet: &CSetCursorItem) {}

        fn enqueue_property_packet(&self, _packet: &CSetContainerProperty) {}

        fn enqueue_slot_set_packet(&self, _packet: &CSetPlayerInventory) {}

        fn enqueue_set_held_item_packet(&self, _packet: &CSetSelectedSlot) {}

        fn enqueue_equipment_change(&self, _slot: &EquipmentSlot, _stack: &ItemStack) {}

        fn award_experience(&self, _amount: i32) {}

        fn increment_stat(&self, _category: StatisticCategory, _stat_id: i32, _amount: i32) {}

        fn play_block_sound(&self, _sound: Sound, _pitch: f32) {}
    }

    fn new_inventory() -> Arc<PlayerInventory> {
        Arc::new(PlayerInventory::new(
            Arc::new(Mutex::new(EntityEquipment::new())),
            Arc::new(crate::build_equipment_slots()),
        ))
    }

    fn new_handler(
        use_crafting_table: bool,
        inventory: &Arc<PlayerInventory>,
    ) -> Box<dyn ScreenHandler> {
        if use_crafting_table {
            Box::new(CraftingTableScreenHandler::new(1, inventory, None))
        } else {
            Box::new(PlayerScreenHandler::new(inventory, None, 1, None))
        }
    }

    fn prepare_recipe(handler: &mut dyn ScreenHandler, ingredient_count: u8) {
        handler.get_behaviour().slots[1].set_stack(ItemStack::new(
            ingredient_count,
            &pumpkin_data::item::Item::OAK_LOG,
        ));
        handler.update_to_client();
        assert_eq!(handler.get_behaviour().slots[0].get_stack().item_count, 4);
    }

    fn prepare_cake_recipe(handler: &mut dyn ScreenHandler) {
        use pumpkin_data::item::Item;

        for slot in [1, 2, 3] {
            handler.get_behaviour().slots[slot].set_stack(ItemStack::new(1, &Item::MILK_BUCKET));
        }
        handler.get_behaviour().slots[4].set_stack(ItemStack::new(1, &Item::SUGAR));
        handler.get_behaviour().slots[5].set_stack(ItemStack::new(1, &Item::EGG));
        handler.get_behaviour().slots[6].set_stack(ItemStack::new(1, &Item::SUGAR));
        for slot in [7, 8, 9] {
            handler.get_behaviour().slots[slot].set_stack(ItemStack::new(1, &Item::WHEAT));
        }
        handler.update_to_client();
        assert_eq!(
            handler.get_behaviour().slots[0].get_stack().item,
            &Item::CAKE
        );
    }

    fn owned_output_total(
        handler: &dyn ScreenHandler,
        inventory: &Arc<PlayerInventory>,
        player: &DropRecorder,
    ) -> u32 {
        let cursor = handler.get_behaviour().cursor_stack.lock().unwrap().clone();
        inventory.count_item(&pumpkin_data::item::Item::OAK_PLANKS)
            + u32::from(cursor.item_count)
            + player.dropped_count(&pumpkin_data::item::Item::OAK_PLANKS)
    }

    fn assert_all_main_stacks_within_limit(inventory: &Arc<PlayerInventory>) {
        for slot in 0..PlayerInventory::MAIN_SIZE {
            let stack = inventory.get_slot(slot);
            assert!(stack.item_count <= stack.get_max_stack_size());
        }
    }

    #[test]
    fn player_screen_partial_result_shift_click_preserves_output() {
        run_partial_result_shift_click(false);
    }

    #[test]
    fn crafting_table_partial_result_shift_click_preserves_output() {
        run_partial_result_shift_click(true);
    }

    fn run_partial_result_shift_click(use_crafting_table: bool) {
        let inventory = new_inventory();
        inventory.set_slot(0, ItemStack::new(62, &pumpkin_data::item::Item::OAK_PLANKS));
        for slot in 1..PlayerInventory::MAIN_SIZE {
            inventory.set_slot(
                slot,
                ItemStack::new(64, &pumpkin_data::item::Item::COBBLESTONE),
            );
        }
        let player = DropRecorder::new(inventory.clone());
        let mut handler = new_handler(use_crafting_table, &inventory);
        prepare_recipe(handler.as_mut(), 1);

        let before = owned_output_total(handler.as_ref(), &inventory, &player);
        handler.on_slot_click(0, 0, SlotActionType::QuickMove, &player);

        assert_eq!(
            owned_output_total(handler.as_ref(), &inventory, &player),
            before + 4
        );
        assert_eq!(inventory.get_slot(0).item_count, 64);
        assert_eq!(
            player.dropped_count(&pumpkin_data::item::Item::OAK_PLANKS),
            2
        );
        assert!(handler.get_behaviour().slots[0].get_stack().is_empty());
        assert!(handler.get_behaviour().slots[1].get_stack().is_empty());
        assert!(
            handler
                .get_behaviour()
                .cursor_stack
                .lock()
                .unwrap()
                .is_empty()
        );
        assert_all_main_stacks_within_limit(&inventory);
    }

    #[test]
    fn player_screen_partial_empty_result_shift_click_preserves_output() {
        run_partial_empty_result_shift_click(false);
    }

    #[test]
    fn crafting_table_partial_empty_result_shift_click_preserves_output() {
        run_partial_empty_result_shift_click(true);
    }

    fn run_partial_empty_result_shift_click(use_crafting_table: bool) {
        let inventory = new_inventory();
        inventory.set_slot(0, ItemStack::new(62, &pumpkin_data::item::Item::OAK_PLANKS));
        for slot in 1..(PlayerInventory::MAIN_SIZE - 1) {
            inventory.set_slot(
                slot,
                ItemStack::new(64, &pumpkin_data::item::Item::COBBLESTONE),
            );
        }
        let player = DropRecorder::new(inventory.clone());
        let mut handler = new_handler(use_crafting_table, &inventory);
        prepare_recipe(handler.as_mut(), 1);

        let before = owned_output_total(handler.as_ref(), &inventory, &player);
        handler.on_slot_click(0, 0, SlotActionType::QuickMove, &player);

        assert_eq!(
            owned_output_total(handler.as_ref(), &inventory, &player),
            before + 4
        );
        assert_eq!(inventory.get_slot(0).item_count, 64);
        assert_eq!(
            inventory
                .get_slot(PlayerInventory::MAIN_SIZE - 1)
                .item_count,
            2
        );
        assert_eq!(
            player.dropped_count(&pumpkin_data::item::Item::OAK_PLANKS),
            0
        );
        assert!(handler.get_behaviour().slots[0].get_stack().is_empty());
        assert!(handler.get_behaviour().slots[1].get_stack().is_empty());
        assert_all_main_stacks_within_limit(&inventory);
    }

    #[test]
    fn player_screen_full_inventory_does_not_consume_recipe() {
        run_full_inventory_result_shift_click(false);
    }

    #[test]
    fn crafting_table_full_inventory_does_not_consume_recipe() {
        run_full_inventory_result_shift_click(true);
    }

    fn run_full_inventory_result_shift_click(use_crafting_table: bool) {
        let inventory = new_inventory();
        for slot in 0..PlayerInventory::MAIN_SIZE {
            inventory.set_slot(
                slot,
                ItemStack::new(64, &pumpkin_data::item::Item::COBBLESTONE),
            );
        }
        let player = DropRecorder::new(inventory.clone());
        let mut handler = new_handler(use_crafting_table, &inventory);
        prepare_recipe(handler.as_mut(), 1);

        handler.on_slot_click(0, 0, SlotActionType::QuickMove, &player);

        assert_eq!(
            inventory.count_item(&pumpkin_data::item::Item::OAK_PLANKS),
            0
        );
        assert_eq!(
            player.dropped_count(&pumpkin_data::item::Item::OAK_PLANKS),
            0
        );
        assert_eq!(handler.get_behaviour().slots[0].get_stack().item_count, 4);
        assert_eq!(handler.get_behaviour().slots[1].get_stack().item_count, 1);
        assert!(
            handler
                .get_behaviour()
                .cursor_stack
                .lock()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn player_screen_repeated_result_shift_click_consumes_all_available_recipes() {
        run_repeated_result_shift_click(false);
    }

    #[test]
    fn crafting_table_repeated_result_shift_click_consumes_all_available_recipes() {
        run_repeated_result_shift_click(true);
    }

    fn run_repeated_result_shift_click(use_crafting_table: bool) {
        let inventory = new_inventory();
        let player = DropRecorder::new(inventory.clone());
        let mut handler = new_handler(use_crafting_table, &inventory);
        prepare_recipe(handler.as_mut(), 2);

        handler.on_slot_click(0, 0, SlotActionType::QuickMove, &player);
        assert_eq!(owned_output_total(handler.as_ref(), &inventory, &player), 8);
        assert_eq!(
            inventory.count_item(&pumpkin_data::item::Item::OAK_PLANKS),
            8
        );
        assert!(handler.get_behaviour().slots[1].get_stack().is_empty());
        assert!(handler.get_behaviour().slots[0].get_stack().is_empty());

        handler.on_slot_click(0, 0, SlotActionType::QuickMove, &player);

        assert_eq!(owned_output_total(handler.as_ref(), &inventory, &player), 8);
        assert!(handler.get_behaviour().slots[0].get_stack().is_empty());
        assert!(handler.get_behaviour().slots[1].get_stack().is_empty());
        assert_eq!(
            player.dropped_count(&pumpkin_data::item::Item::OAK_PLANKS),
            0
        );
        assert_all_main_stacks_within_limit(&inventory);
    }

    #[test]
    fn cake_pickup_returns_bucket_remainders_and_close_returns_cursor() {
        // Cake is a 3x3 recipe and is only valid in the crafting-table handler;
        // the player inventory handler intentionally exposes a 2x2 grid.
        run_cake_pickup_remainder(true);
    }

    fn run_cake_pickup_remainder(use_crafting_table: bool) {
        use pumpkin_data::item::Item;

        let inventory = new_inventory();
        let player = DropRecorder::new(inventory.clone());
        let mut handler = new_handler(use_crafting_table, &inventory);
        prepare_cake_recipe(handler.as_mut());

        handler.on_slot_click(0, 0, SlotActionType::Pickup, &player);

        let cursor = handler.get_behaviour().cursor_stack.lock().unwrap().clone();
        assert_eq!(cursor.item, &Item::CAKE);
        assert_eq!(cursor.item_count, 1);
        assert!(handler.get_behaviour().slots[0].get_stack().is_empty());
        for slot in [1, 2, 3] {
            assert_eq!(
                handler.get_behaviour().slots[slot].get_stack().item,
                &Item::BUCKET
            );
            assert_eq!(
                handler.get_behaviour().slots[slot].get_stack().item_count,
                1
            );
        }
        for slot in [4, 5, 6, 7, 8, 9] {
            assert!(handler.get_behaviour().slots[slot].get_stack().is_empty());
        }

        handler.on_closed(&player);

        assert!(
            handler
                .get_behaviour()
                .cursor_stack
                .lock()
                .unwrap()
                .is_empty()
        );
        assert_eq!(inventory.count_item(&Item::CAKE), 1);
        assert_eq!(inventory.count_item(&Item::BUCKET), 3);
        assert_eq!(player.dropped_count(&Item::CAKE), 0);
        assert_eq!(player.dropped_count(&Item::BUCKET), 0);
    }

    #[test]
    fn map_transmute_uses_material_slot_count_and_preserves_components() {
        use pumpkin_data::item::Item;

        for material_slots in [1, 2, 8] {
            assert_map_transmute(material_slots, false, 1 + material_slots as u8);
        }
        // A stack of eight maps in one slot is one occupied material slot, not
        // eight materials. The handler must consume only one map from it.
        assert_map_transmute(1, true, 2);

        // A 3x3 handler can represent nine occupied material-like slots only
        // without the required filled-map input; that input must not be
        // inferred from the occupied count.
        let inventory = new_inventory();
        let invalid_player = DropRecorder::new(inventory.clone());
        let mut invalid_handler = new_handler(true, &inventory);
        for slot in 1..=9 {
            invalid_handler.get_behaviour().slots[slot].set_stack(ItemStack::new(1, &Item::MAP));
        }
        invalid_handler.update_to_client();
        assert!(
            invalid_handler.get_behaviour().slots[0]
                .get_stack()
                .is_empty()
        );
        invalid_handler.on_slot_click(0, 0, SlotActionType::QuickMove, &invalid_player);
        assert!(
            invalid_handler
                .get_behaviour()
                .cursor_stack
                .lock()
                .unwrap()
                .is_empty()
        );

        // Input/input is not a transmute, even though both slots are occupied.
        let invalid_inventory = new_inventory();
        let invalid_player = DropRecorder::new(invalid_inventory.clone());
        let mut invalid_handler = new_handler(true, &invalid_inventory);
        let mut first_map = ItemStack::new(1, &Item::FILLED_MAP);
        first_map.set_data_component(MapIdImpl { id: 7 });
        invalid_handler.get_behaviour().slots[1].set_stack(first_map);
        invalid_handler.get_behaviour().slots[2].set_stack(ItemStack::new(1, &Item::FILLED_MAP));
        invalid_handler.update_to_client();
        assert!(
            invalid_handler.get_behaviour().slots[0]
                .get_stack()
                .is_empty()
        );
        invalid_handler.on_slot_click(0, 0, SlotActionType::QuickMove, &invalid_player);
        assert!(
            invalid_handler
                .get_behaviour()
                .cursor_stack
                .lock()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn binding_curse_armor_cannot_be_removed_in_survival() {
        use pumpkin_data::{Enchantment, item::Item};

        let inventory = new_inventory();
        let player = DropRecorder::new(inventory.clone());
        let mut helmet = ItemStack::new(1, &Item::DIAMOND_HELMET);
        helmet.add_enchantment(&Enchantment::BINDING_CURSE, 1);
        inventory.set_slot(39, helmet);
        let mut handler = new_handler(false, &inventory);

        assert!(!handler.get_behaviour().slots[5].can_take_items(&player));
        handler.on_slot_click(5, 0, SlotActionType::Pickup, &player);

        assert!(!handler.get_behaviour().slots[5].get_stack().is_empty());
        assert!(
            handler
                .get_behaviour()
                .cursor_stack
                .lock()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn throw_all_binding_curse_armor_returns_without_dropping() {
        use pumpkin_data::{Enchantment, item::Item};

        let inventory = new_inventory();
        let player = DropRecorder::new(inventory.clone());
        let mut helmet = ItemStack::new(1, &Item::DIAMOND_HELMET);
        helmet.add_enchantment(&Enchantment::BINDING_CURSE, 1);
        inventory.set_slot(39, helmet);
        let mut handler = new_handler(false, &inventory);

        assert!(!handler.get_behaviour().slots[5].can_take_items(&player));
        handler.on_slot_click(5, 1, SlotActionType::Throw, &player);

        assert_eq!(
            handler.get_behaviour().slots[5].get_stack().item,
            &Item::DIAMOND_HELMET
        );
        assert!(player.drops.lock().unwrap().is_empty());
    }

    fn assert_map_transmute(material_slots: usize, stacked_material: bool, expected_count: u8) {
        use pumpkin_data::item::Item;

        let inventory = new_inventory();
        let player = DropRecorder::new(inventory.clone());
        let mut handler = new_handler(true, &inventory);
        let mut filled_map = ItemStack::new(1, &Item::FILLED_MAP);
        filled_map.set_data_component(MapIdImpl { id: 42 });
        handler.get_behaviour().slots[1].set_stack(filled_map);
        for offset in 0..material_slots {
            let stack_count = if stacked_material && offset == 0 {
                8
            } else {
                1
            };
            handler.get_behaviour().slots[2 + offset]
                .set_stack(ItemStack::new(stack_count, &Item::MAP));
        }
        handler.update_to_client();

        let preview = handler.get_behaviour().slots[0].get_stack();
        assert_eq!(preview.item, &Item::FILLED_MAP);
        assert_eq!(preview.item_count, expected_count);
        assert_eq!(preview.get_data_component::<MapIdImpl>().unwrap().id, 42);

        handler.on_slot_click(0, 0, SlotActionType::Pickup, &player);
        let cursor = handler.get_behaviour().cursor_stack.lock().unwrap().clone();
        assert_eq!(cursor.item_count, expected_count);
        assert_eq!(cursor.get_data_component::<MapIdImpl>().unwrap().id, 42);
        assert!(handler.get_behaviour().slots[1].get_stack().is_empty());
        for offset in 0..material_slots {
            let stack = handler.get_behaviour().slots[2 + offset].get_stack();
            if stacked_material && offset == 0 {
                assert_eq!(stack.item, &Item::MAP);
                assert_eq!(stack.item_count, 7);
            } else {
                assert!(stack.is_empty());
            }
        }
    }
}
