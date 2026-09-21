use std::any::Any;
use std::collections::BTreeMap;

use crate::block::registry::BlockActionResult;
use crate::entity::EntityBase;
use crate::entity::player::Player;
use crate::item::{ItemBehaviour, ItemMetadata};
use crate::server::Server;
use pumpkin_data::data_component_impl::DebugStickStateImpl;
use pumpkin_data::item::Item;
use pumpkin_data::item_stack::ItemStack;
use pumpkin_data::{Block, BlockDirection, BlockStateId};
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector3::Vector3;
use pumpkin_util::text::TextComponent;
use pumpkin_world::world::BlockFlags;

pub struct DebugStickItem;

impl ItemMetadata for DebugStickItem {
    fn ids() -> Box<[u16]> {
        Box::new([Item::DEBUG_STICK.id])
    }
}

impl DebugStickItem {
    fn selected_property(
        item: &ItemStack,
        block: &Block,
        prop_names: &[&'static str],
    ) -> &'static str {
        let key = format!("minecraft:{}", block.name);
        item.get_data_component::<DebugStickStateImpl>()
            .and_then(|state| state.properties.get(&key))
            .and_then(|selected| {
                prop_names
                    .iter()
                    .find(|name| **name == selected.as_str())
                    .copied()
            })
            .unwrap_or(prop_names[0])
    }

    fn set_selected_property(item: &mut ItemStack, block: &Block, property: &'static str) {
        let key = format!("minecraft:{}", block.name);
        if let Some(state) = item.get_data_component_mut::<DebugStickStateImpl>() {
            state.properties.insert(key, property.to_owned());
        } else {
            let mut properties = BTreeMap::new();
            properties.insert(key, property.to_owned());
            item.set_data_component(DebugStickStateImpl { properties });
        }
    }

    fn handle_interaction(
        item: &mut ItemStack,
        player: &Player,
        pos: &BlockPos,
        block: &Block,
        state_id: BlockStateId,
        cycle: bool,
    ) -> bool {
        let Some(props) = block.properties(state_id) else {
            player.send_system_message_raw(
                &TextComponent::translate(
                    "item.minecraft.debug_stick.empty",
                    [TextComponent::text(block.name)],
                ),
                true,
            );
            return false;
        };

        let prop_list = props.to_props();
        if prop_list.is_empty() {
            player.send_system_message_raw(
                &TextComponent::translate(
                    "item.minecraft.debug_stick.empty",
                    [TextComponent::text(block.name)],
                ),
                true,
            );
            return false;
        }

        let prop_names: Vec<&'static str> = prop_list.iter().map(|(k, _)| *k).collect();
        let selected_prop = Self::selected_property(item, block, &prop_names);

        if cycle {
            let mut possible_values: Vec<&'static str> = Vec::new();
            for state in block.states {
                if let Some(state_props) = block.properties(state.id) {
                    for (k, v) in state_props.to_props() {
                        if k == selected_prop && !possible_values.contains(&v) {
                            possible_values.push(v);
                        }
                    }
                }
            }

            if possible_values.is_empty() {
                return false;
            }

            let current_val = prop_list
                .iter()
                .find(|(k, _)| *k == selected_prop)
                .map_or(possible_values[0], |(_, v)| *v);
            let cur_idx = possible_values
                .iter()
                .position(|v| *v == current_val)
                .unwrap_or(0);
            let is_backward = player.get_entity().is_sneaking();
            let new_idx = if is_backward {
                (cur_idx + possible_values.len() - 1) % possible_values.len()
            } else {
                (cur_idx + 1) % possible_values.len()
            };
            let new_val = possible_values[new_idx];

            let mut new_props = prop_list.clone();
            for (k, v) in &mut new_props {
                if *k == selected_prop {
                    *v = new_val;
                }
            }

            let new_state_id = block.from_properties(&new_props).to_state_id(block);
            let world = player.world();
            world.set_block_state(pos, new_state_id, BlockFlags::NOTIFY_ALL);

            player.send_system_message_raw(
                &TextComponent::translate(
                    "item.minecraft.debug_stick.update",
                    [
                        TextComponent::text(selected_prop),
                        TextComponent::text(new_val),
                    ],
                ),
                true,
            );
        } else {
            let cur_idx = prop_names
                .iter()
                .position(|name| *name == selected_prop)
                .unwrap_or(0);
            let is_backward = player.get_entity().is_sneaking();
            let new_idx = if is_backward {
                (cur_idx + prop_names.len() - 1) % prop_names.len()
            } else {
                (cur_idx + 1) % prop_names.len()
            };
            let new_selected_prop = prop_names[new_idx];
            Self::set_selected_property(item, block, new_selected_prop);

            let current_val = prop_list
                .iter()
                .find(|(k, _)| *k == new_selected_prop)
                .map_or("", |(_, v)| *v);

            player.send_system_message_raw(
                &TextComponent::translate(
                    "item.minecraft.debug_stick.select",
                    [
                        TextComponent::text(new_selected_prop),
                        TextComponent::text(current_val),
                    ],
                ),
                true,
            );
        }

        true
    }
}

impl ItemBehaviour for DebugStickItem {
    fn can_mine_with_stack(&self, item: &mut ItemStack, player: &Player) -> bool {
        if player.can_use_game_master_blocks() {
            let (start, end) = self.get_start_and_end_pos(player);
            let world = player.world();
            if let Some((pos, _)) =
                world.raycast(start, end, |pos, world| world.get_block(pos) != &Block::AIR)
            {
                let (block, state) = world.get_block_and_state(&pos);
                Self::handle_interaction(item, player, &pos, block, state.id, false);
            }
        }
        false
    }

    fn use_on_block(
        &self,
        item: &mut ItemStack,
        player: &Player,
        location: BlockPos,
        _face: BlockDirection,
        _cursor_pos: Vector3<f32>,
        block: &Block,
        _server: &Server,
    ) -> BlockActionResult {
        if player.can_use_game_master_blocks() {
            let world = player.world();
            let state_id = world.get_block_state_id(&location);
            if Self::handle_interaction(item, player, &location, block, state_id, true) {
                return BlockActionResult::Success;
            }
        }
        BlockActionResult::Fail
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_data::data_component::DataComponent;
    use pumpkin_nbt::compound::NbtCompound;
    use pumpkin_nbt::tag::NbtTag;

    fn property_names(block: &Block) -> Vec<&'static str> {
        block
            .properties(block.default_state.id)
            .unwrap_or_else(|| panic!("{} has no properties", block.name))
            .to_props()
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    #[test]
    fn selected_property_is_item_local_and_restored_component_drives_decision() {
        let block = &Block::OAK_DOOR;
        let names = property_names(block);
        assert!(names.len() > 1);

        let mut first = ItemStack::new(1, &Item::DEBUG_STICK);
        let mut second = ItemStack::new(1, &Item::DEBUG_STICK);
        DebugStickItem::set_selected_property(&mut first, block, names[0]);
        DebugStickItem::set_selected_property(&mut second, block, names[1]);

        assert_eq!(
            DebugStickItem::selected_property(&first, block, &names),
            names[0]
        );
        assert_eq!(
            DebugStickItem::selected_property(&second, block, &names),
            names[1]
        );
        let key = format!("minecraft:{}", block.name);
        let mut compound = NbtCompound::new();
        compound.put_string(&key, names[1].to_owned());
        let restored_state = DebugStickStateImpl::read_data(&NbtTag::Compound(compound))
            .expect("debug-stick component should restore");
        let restored = ItemStack::new_with_component(
            1,
            &Item::DEBUG_STICK,
            vec![(
                DataComponent::DebugStickState,
                Some(Box::new(restored_state)),
            )],
        );
        assert_eq!(
            DebugStickItem::selected_property(&restored, block, &names),
            names[1]
        );

        let other_block = &Block::OAK_LOG;
        let other_names = property_names(other_block);
        assert_eq!(
            DebugStickItem::selected_property(&first, other_block, &other_names),
            other_names[0]
        );
    }

    #[test]
    fn invalid_or_missing_selected_property_falls_back_to_first_in_block_order() {
        let block = &Block::OAK_DOOR;
        let names = property_names(block);
        let mut stack = ItemStack::new(1, &Item::DEBUG_STICK);
        DebugStickItem::set_selected_property(&mut stack, block, "not_a_property");
        assert_eq!(
            DebugStickItem::selected_property(&stack, block, &names),
            names[0]
        );
    }
}
