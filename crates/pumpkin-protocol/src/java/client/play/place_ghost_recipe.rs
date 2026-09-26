use pumpkin_data::packet::clientbound::play::PLACE_GHOST_RECIPE;
use pumpkin_macros::java_packet;

use crate::{ClientPacket, ser::NetworkWriteExt};
use pumpkin_util::version::JavaMinecraftVersion;

use super::recipe_book_add::{RecipeDisplay, SlotDisplay, write_recipe_display};

#[java_packet(PLACE_GHOST_RECIPE)]
pub struct CPlaceGhostRecipe<'a> {
    pub window_id: u8,
    pub recipe_id: &'a str,
    pub display: Option<&'a RecipeDisplay<'a>>,
}

impl<'a> CPlaceGhostRecipe<'a> {
    /// Legacy recipe-ID packet constructor.
    #[must_use]
    pub const fn new(window_id: u8, recipe_id: &'a str) -> Self {
        Self { window_id, recipe_id, display: None }
    }

    /// Constructor for the typed RecipeDisplay packet used by 26.2.
    #[must_use]
    pub const fn with_display(window_id: u8, display: &'a RecipeDisplay<'a>) -> Self {
        Self { window_id, recipe_id: "", display: Some(display) }
    }
}

impl ClientPacket for CPlaceGhostRecipe<'_> {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), crate::ser::WritingError> {
        if self.display.is_some() && *version < JavaMinecraftVersion::V_26_2 {
            return Err(crate::ser::WritingError::Message(
                "Typed PlaceGhostRecipe requires 26.2 or newer".into(),
            ));
        }
        write.write_container_id(&crate::VarInt(i32::from(self.window_id)), version)?;
        if *version >= JavaMinecraftVersion::V_26_2 {
            let display = self.display.ok_or_else(|| crate::ser::WritingError::Message(
                "26.2 PlaceGhostRecipe requires a typed RecipeDisplay; string adapter is unsupported".into(),
            ))?;
            write_recipe_display(&mut write, display, *version)?;
        } else {
            write.write_string(self.recipe_id)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ser::{NetworkReadExt, WritingError};
    use pumpkin_data::item::Item;
    use std::io::Cursor;

    fn encode(display: &RecipeDisplay<'_>) -> Vec<u8> {
        let packet = CPlaceGhostRecipe::with_display(2, display);
        let mut bytes = Vec::new();
        packet.write_packet_data(&mut bytes, &JavaMinecraftVersion::V_26_2)
            .expect("encode ghost recipe");
        bytes
    }

    fn check_varints(bytes: &[u8], expected: &[i32]) {
        let mut cursor = Cursor::new(bytes);
        for expected in expected {
            assert_eq!(cursor.get_var_int().expect("decode VarInt").0, *expected);
        }
        assert_eq!(cursor.position() as usize, bytes.len());
    }

    #[test]
    fn typed_display_is_rejected_before_26_2() {
        let stone = Item::from_registry_key("stone").expect("stone item");
        let display = RecipeDisplay::Shapeless {
            ingredients: vec![SlotDisplay::Item(stone)],
            result: SlotDisplay::Item(stone),
            crafting_station: SlotDisplay::Empty,
        };
        let packet = CPlaceGhostRecipe::with_display(2, &display);
        assert!(matches!(
            packet.write_packet_data(Vec::new(), &JavaMinecraftVersion::V_26_1),
            Err(WritingError::Message(_))
        ));
    }

    #[test]
    fn legacy_recipe_26_1_golden() {
        let packet = CPlaceGhostRecipe::new(2, "minecraft:stone");
        let mut bytes = Vec::new();
        packet.write_packet_data(&mut bytes, &JavaMinecraftVersion::V_26_1)
            .expect("encode legacy ghost recipe");
        assert_eq!(bytes, b"\x02\x0fminecraft:stone");
    }

    #[test]
    fn shapeless_display_26_2_golden() {
        let stone = Item::from_registry_key("stone").expect("stone item");
        let display = RecipeDisplay::Shapeless {
            ingredients: vec![SlotDisplay::Item(stone), SlotDisplay::Item(stone)],
            result: SlotDisplay::Item(stone),
            crafting_station: SlotDisplay::Empty,
        };
        let golden = [2, 0, 2, 4, 1, 4, 1, 4, 1, 0];
        assert_eq!(encode(&display), golden);
        check_varints(&golden, &[2, 0, 2, 4, 1, 4, 1, 4, 1, 0]);
    }

    #[test]
    fn shaped_display_26_2_golden_and_legacy_adapter_is_unsupported() {
        let stone = Item::from_registry_key("stone").expect("stone item");
        let display = RecipeDisplay::Shaped {
            width: 2,
            height: 1,
            ingredients: vec![SlotDisplay::Item(stone), SlotDisplay::Empty],
            result: SlotDisplay::Item(stone),
            crafting_station: SlotDisplay::Empty,
        };
        let golden = [2, 1, 2, 1, 2, 4, 1, 0, 4, 1, 0];
        let encoded = encode(&display);
        assert_eq!(encoded, golden);

        // ShapedCraftingRecipeDisplay.STREAM_CODEC writes width, height, then
        // a length-prefixed SlotDisplay list, followed by result and station.
        let mut cursor = Cursor::new(encoded.as_slice());
        assert_eq!(cursor.get_var_int().unwrap().0, 2); // window id
        assert_eq!(cursor.get_var_int().unwrap().0, 1); // shaped display type
        assert_eq!(cursor.get_var_int().unwrap().0, 2); // width
        assert_eq!(cursor.get_var_int().unwrap().0, 1); // height
        assert_eq!(cursor.get_var_int().unwrap().0, 2); // ingredient count
        assert_eq!(cursor.get_var_int().unwrap().0, 4); // first ingredient: item
        assert_eq!(cursor.get_var_int().unwrap().0, stone.id as i32);
        assert_eq!(cursor.get_var_int().unwrap().0, 0); // second ingredient: empty
        assert_eq!(cursor.get_var_int().unwrap().0, 4); // result: item
        assert_eq!(cursor.get_var_int().unwrap().0, stone.id as i32);
        assert_eq!(cursor.get_var_int().unwrap().0, 0); // crafting station: empty
        assert_eq!(cursor.position() as usize, encoded.len());

        let legacy = CPlaceGhostRecipe::new(2, "minecraft:stone");
        assert!(matches!(
            legacy.write_packet_data(Vec::new(), &JavaMinecraftVersion::V_26_2),
            Err(WritingError::Message(_))
        ));
    }
}
