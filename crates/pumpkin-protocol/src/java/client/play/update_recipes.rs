use crate::{
    ClientPacket, ServerPacket, VarInt,
    codec::item_stack_seralizer::ItemStackTemplateSerializer,
    ser::{NetworkReadSliceExt, NetworkWriteExt, ReadingError, WritingError},
};
use pumpkin_data::{
    item::Item,
    item_id_remap::remap_item_id_for_version,
    item_stack::ItemStack,
    packet::clientbound::play::UPDATE_RECIPES,
    recipes::{
        CookingRecipeType, RECIPES_COOKING, RECIPES_SMITHING_TRANSFORM, RECIPES_SMITHING_TRIM,
        RECIPES_STONECUTTING, RecipeIngredientTypes,
    },
    slot_display_id_remap::remap_slot_display_id_for_version,
    tag::Taggable,
};
use pumpkin_macros::java_packet;
use pumpkin_util::version::JavaMinecraftVersion;
use std::{collections::BTreeMap, io::Write};

const COOKING_KEYS: [(&str, fn(&CookingRecipeType) -> bool); 4] = [
    ("minecraft:furnace_input", |r| {
        matches!(r, CookingRecipeType::Smelting(_))
    }),
    ("minecraft:blast_furnace_input", |r| {
        matches!(r, CookingRecipeType::Blasting(_))
    }),
    ("minecraft:smoker_input", |r| {
        matches!(r, CookingRecipeType::Smoking(_))
    }),
    ("minecraft:campfire_input", |r| {
        matches!(r, CookingRecipeType::CampfireCooking(_))
    }),
];
const SLOT_DISPLAY_ITEM_STACK: u32 = 5;

#[java_packet(UPDATE_RECIPES)]
pub struct CUpdateRecipes<'a> {
    pub raw_data: &'a [u8],
    generated_vanilla: bool,
}

impl<'a> CUpdateRecipes<'a> {
    #[must_use]
    pub const fn new(raw_data: &'a [u8]) -> Self {
        Self {
            raw_data,
            generated_vanilla: false,
        }
    }

    #[must_use]
    pub const fn generated_vanilla() -> Self {
        Self {
            raw_data: &[],
            generated_vanilla: true,
        }
    }
}

impl ClientPacket for CUpdateRecipes<'_> {
    fn write_packet_data(
        &self,
        mut write: impl Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        if self.generated_vanilla {
            write_generated_vanilla(&mut write, *version)
        } else {
            write.write_slice(self.raw_data)?;
            Ok(())
        }
    }
}

impl<'a> ServerPacket<'a> for CUpdateRecipes<'a> {
    fn read(bytebuf: &mut &'a [u8], _version: &JavaMinecraftVersion) -> Result<Self, ReadingError> {
        Ok(Self::new(
            bytebuf.read_remaining_slice_borrowed(usize::MAX)?,
        ))
    }
}

fn resolve_ingredient(
    ingredient: &RecipeIngredientTypes,
) -> Result<Vec<&'static Item>, WritingError> {
    let ids: Vec<&str> = match ingredient {
        RecipeIngredientTypes::Simple(id) => vec![id],
        RecipeIngredientTypes::OneOf(ids) => ids.to_vec(),
        RecipeIngredientTypes::Tagged(tag) => {
            let tag = tag.strip_prefix('#').unwrap_or(tag);
            let tag = tag.strip_prefix("minecraft:").unwrap_or(tag);
            Item::get_tag_values(tag)
                .ok_or_else(|| WritingError::Message(format!("Unknown recipe item tag: {tag}")))?
                .iter()
                .copied()
                .collect()
        }
    };
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        let key = id.strip_prefix("minecraft:").unwrap_or(id);
        let item = Item::from_registry_key(key)
            .ok_or_else(|| WritingError::Message(format!("Unknown recipe item: {id}")))?;
        if !items
            .iter()
            .any(|candidate: &&Item| candidate.id == item.id)
        {
            items.push(item);
        }
    }
    items.sort_unstable_by_key(|item| item.id);
    if items.is_empty() {
        return Err(WritingError::Message(
            "Recipe ingredient resolved to no items".into(),
        ));
    }
    Ok(items)
}

fn write_item_set(
    write: &mut impl NetworkWriteExt,
    items: &[&Item],
    version: JavaMinecraftVersion,
) -> Result<(), WritingError> {
    write.write_var_int(&VarInt(items.len() as i32))?;
    for item in items {
        write.write_var_int(&VarInt(remap_item_id_for_version(item.id, version) as i32))?;
    }
    Ok(())
}

fn write_ingredient(
    write: &mut impl NetworkWriteExt,
    ingredient: &RecipeIngredientTypes,
    version: JavaMinecraftVersion,
) -> Result<(), WritingError> {
    if let RecipeIngredientTypes::Tagged(tag) = ingredient {
        let tag = tag.strip_prefix('#').unwrap_or(tag);
        let tag_path = tag.strip_prefix("minecraft:").unwrap_or(tag);
        if Item::get_tag_values(tag_path).is_none() {
            return Err(WritingError::Message(format!(
                "Unknown recipe item tag: {tag}"
            )));
        }
        let tag = if tag.contains(':') {
            tag.to_owned()
        } else {
            format!("minecraft:{tag}")
        };
        // ByteBufCodecs.holderSet: 0 marks a named holder set, then its identifier.
        write.write_var_int(&VarInt(0))?;
        write.write_string(&tag)?;
        return Ok(());
    }
    let items = resolve_ingredient(ingredient)?;
    write.write_var_int(&VarInt(items.len() as i32 + 1))?;
    for item in items {
        write.write_var_int(&VarInt(remap_item_id_for_version(item.id, version) as i32))?;
    }
    Ok(())
}

fn write_generated_vanilla(
    write: &mut impl NetworkWriteExt,
    version: JavaMinecraftVersion,
) -> Result<(), WritingError> {
    if version != JavaMinecraftVersion::V_26_2 {
        return Err(WritingError::Message(
            "Generated recipe property sets are only valid for Minecraft 26.2".into(),
        ));
    }

    let mut property_sets = BTreeMap::<&str, Vec<&Item>>::new();
    for (key, matches) in COOKING_KEYS {
        let mut items = Vec::new();
        for recipe in RECIPES_COOKING.iter().filter(|recipe| matches(recipe)) {
            let ingredient = match recipe {
                CookingRecipeType::Blasting(r)
                | CookingRecipeType::Smelting(r)
                | CookingRecipeType::Smoking(r)
                | CookingRecipeType::CampfireCooking(r) => &r.ingredient,
            };
            items.extend(resolve_ingredient(ingredient)?);
        }
        items.sort_unstable_by_key(|item| item.id);
        items.dedup_by_key(|item| item.id);
        property_sets.insert(key, items);
    }

    for (key, slot) in [
        ("minecraft:smithing_template", 0),
        ("minecraft:smithing_base", 1),
        ("minecraft:smithing_addition", 2),
    ] {
        let mut items = Vec::new();
        for recipe in RECIPES_SMITHING_TRANSFORM {
            let ingredient = match slot {
                0 => &recipe.template,
                1 => &recipe.base,
                _ => &recipe.addition,
            };
            items.extend(resolve_ingredient(ingredient)?);
        }
        for recipe in RECIPES_SMITHING_TRIM {
            let ingredient = match slot {
                0 => &recipe.template,
                1 => &recipe.base,
                _ => &recipe.addition,
            };
            items.extend(resolve_ingredient(ingredient)?);
        }
        items.sort_unstable_by_key(|item| item.id);
        items.dedup_by_key(|item| item.id);
        property_sets.insert(key, items);
    }

    write.write_var_int(&VarInt(property_sets.len() as i32))?;
    for (key, items) in property_sets {
        write.write_string(key)?;
        write_item_set(write, &items, version)?;
    }

    write.write_var_int(&VarInt(RECIPES_STONECUTTING.len() as i32))?;
    for recipe in RECIPES_STONECUTTING {
        write_ingredient(write, &recipe.ingredient, version)?;
        write.write_var_int(&VarInt(remap_slot_display_id_for_version(
            SLOT_DISPLAY_ITEM_STACK,
            version,
        ) as i32))?;
        let key = recipe
            .result
            .id
            .strip_prefix("minecraft:")
            .unwrap_or(recipe.result.id);
        let item = Item::from_registry_key(key).ok_or_else(|| {
            WritingError::Message(format!(
                "Unknown stonecutter output item: {}",
                recipe.result.id
            ))
        })?;
        ItemStackTemplateSerializer::from(ItemStack::new(recipe.result.count, item))
            .write_with_version(write, &version)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holderset_direct_and_named_tag_fixtures() {
        let version = JavaMinecraftVersion::V_26_2;
        let mut direct = Vec::new();
        write_ingredient(
            &mut direct,
            &RecipeIngredientTypes::Simple("minecraft:stone"),
            version,
        )
        .unwrap();
        assert_eq!(
            direct,
            [2, remap_item_id_for_version(Item::STONE.id, version) as u8]
        );

        let mut tagged = Vec::new();
        write_ingredient(
            &mut tagged,
            &RecipeIngredientTypes::Tagged("#minecraft:logs_that_burn"),
            version,
        )
        .unwrap();
        let mut expected = vec![0, 24]; // named-set marker + identifier byte length
        expected.extend_from_slice(b"minecraft:logs_that_burn");
        assert_eq!(tagged, expected);
    }

    fn read_var_int(bytes: &[u8], offset: &mut usize) -> i32 {
        let mut value = 0;
        let mut shift = 0;
        loop {
            let byte = bytes[*offset];
            *offset += 1;
            value |= i32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return value;
            }
            shift += 7;
        }
    }

    fn read_string<'a>(bytes: &'a [u8], offset: &mut usize) -> &'a str {
        let len = read_var_int(bytes, offset) as usize;
        let string = std::str::from_utf8(&bytes[*offset..*offset + len]).unwrap();
        *offset += len;
        string
    }

    #[test]
    fn generated_26_2_baseline_has_all_property_sets_and_stonecutter_count() {
        let version = JavaMinecraftVersion::V_26_2;
        let mut bytes = Vec::new();
        write_generated_vanilla(&mut bytes, version).unwrap();
        let mut offset = 0;
        assert_eq!(read_var_int(&bytes, &mut offset), 7);
        let mut keys = BTreeMap::new();
        for _ in 0..7 {
            let key = read_string(&bytes, &mut offset).to_owned();
            let count = read_var_int(&bytes, &mut offset);
            let mut ids = Vec::new();
            for _ in 0..count {
                ids.push(read_var_int(&bytes, &mut offset) as u32);
            }
            assert!(keys.insert(key, ids).is_none());
        }
        assert_eq!(keys.len(), 7);
        let smithing_base = &keys["minecraft:smithing_base"];
        let spear_id = remap_item_id_for_version(Item::DIAMOND_SPEAR.id, version) as u32;
        assert!(smithing_base.contains(&spear_id));
        let smithing_addition = &keys["minecraft:smithing_addition"];
        assert!(smithing_addition.contains(&(remap_item_id_for_version(
            Item::NETHERITE_INGOT.id,
            version
        ) as u32)));
        assert!(smithing_base.contains(&(remap_item_id_for_version(Item::IRON_HELMET.id, version) as u32))); // expanded trimmable_armor tag
        assert!(
            !keys
                .values()
                .flatten()
                .any(|id| { *id == remap_item_id_for_version(Item::AIR.id, version) as u32 })
        );
        let stonecutter_start = offset;
        assert_eq!(
            read_var_int(&bytes, &mut offset) as usize,
            RECIPES_STONECUTTING.len()
        );
        let mut expected_stonecutter = Vec::new();
        expected_stonecutter
            .write_var_int(&VarInt(RECIPES_STONECUTTING.len() as i32))
            .unwrap();
        for recipe in RECIPES_STONECUTTING {
            write_ingredient(&mut expected_stonecutter, &recipe.ingredient, version).unwrap();
            expected_stonecutter
                .write_var_int(&VarInt(remap_slot_display_id_for_version(
                    SLOT_DISPLAY_ITEM_STACK,
                    version,
                ) as i32))
                .unwrap();
            let key = recipe
                .result
                .id
                .strip_prefix("minecraft:")
                .unwrap_or(recipe.result.id);
            let item = Item::from_registry_key(key).unwrap();
            ItemStackTemplateSerializer::from(ItemStack::new(recipe.result.count, item))
                .write_with_version(&mut expected_stonecutter, &version)
                .unwrap();
        }
        assert_eq!(&bytes[stonecutter_start..], expected_stonecutter);
    }
}
