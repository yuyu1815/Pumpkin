use crate::VarInt;
use crate::codec::data_component::{DataComponentCodec, deserialize, serialize};
use crate::ser::{NetworkReadExt, NetworkWriteExt, ReadingError, WritingError};
use pumpkin_data::data_component::DataComponent;
use pumpkin_data::data_component_impl::{
    CustomDataImpl, CustomNameImpl, DataComponentImpl, ItemNameImpl,
};
use pumpkin_data::item::Item;
use pumpkin_data::item_id_remap::{remap_item_id_for_version, remap_item_id_from_version};
use pumpkin_data::item_stack::ItemStack;
use pumpkin_nbt::tag::NbtTag;
use pumpkin_util::text::TextComponent;
use pumpkin_util::version::JavaMinecraftVersion;
use std::borrow::Cow;
use std::io::Cursor;

#[derive(Clone)]
pub struct ItemStackSerializer<'a>(pub Cow<'a, ItemStack>);

fn remapped_patch<'a>(
    stack: &'a ItemStack,
    version: JavaMinecraftVersion,
) -> Vec<(u32, DataComponent, Option<&'a dyn DataComponentImpl>)> {
    let mut remapped: Vec<(u32, DataComponent, Option<&'a dyn DataComponentImpl>)> = Vec::new();
    for (id, data) in stack.effective_patch() {
        let wire_id = remap_data_component_type_id_for_version(u32::from(id.to_id()), version);
        if wire_id == 0 && id != DataComponent::CustomData {
            continue;
        }
        if let Some(entry) = remapped.iter_mut().find(|entry| entry.0 == wire_id) {
            entry.1 = id;
            entry.2 = data;
        } else {
            remapped.push((wire_id, id, data));
        }
    }
    remapped
}

fn set_patch_entry(
    patch: &mut Vec<(DataComponent, Option<Box<dyn DataComponentImpl>>)>,
    id: DataComponent,
    value: Option<Box<dyn DataComponentImpl>>,
) {
    if let Some((_, current)) = patch.iter_mut().find(|(patch_id, _)| *patch_id == id) {
        *current = value;
    } else {
        patch.push((id, value));
    }
}

use pumpkin_data::data_component_type_id_remap::{
    remap_data_component_type_id_for_version, remap_data_component_type_id_from_version,
};

fn serialize_item_stack_with_id(
    stack: &ItemStack,
    item_id: u16,
    version: JavaMinecraftVersion,
    write: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    if version >= JavaMinecraftVersion::V_1_20_5 {
        if stack.is_empty() {
            write.put_var_int(&VarInt(0))
        } else {
            let effective = remapped_patch(stack, version);
            let (to_add, to_remove) =
                effective
                    .iter()
                    .fold((0, 0), |(add, remove), (_, _, data)| {
                        if data.is_some() {
                            (add + 1, remove)
                        } else {
                            (add, remove + 1)
                        }
                    });
            write.put_var_int(&VarInt::from(stack.item_count))?;
            write.put_var_int(&VarInt::from(item_id))?;
            write.put_var_int(&VarInt(to_add))?;
            write.put_var_int(&VarInt(to_remove))?;

            for (wire_id, id, data) in &effective {
                if let Some(data) = data {
                    write.put_var_int(&VarInt(*wire_id as i32))?;
                    serialize(*id, *data, write)?;
                }
            }

            for (wire_id, _, data) in &effective {
                if data.is_none() {
                    write.put_var_int(&VarInt(*wire_id as i32))?;
                }
            }

            Ok(())
        }
    } else if version >= JavaMinecraftVersion::V_1_13_2 {
        if stack.is_empty() {
            write.write_bool(false)
        } else {
            write.write_bool(true)?;
            write.put_var_int(&VarInt::from(item_id))?;
            write.write_i8(stack.item_count as i8)?;
            write.write_u8(0)?; // TAG_End (no NBT)
            Ok(())
        }
    } else if version >= JavaMinecraftVersion::V_1_13 {
        // 1.13 and 1.13.1: short id (-1 if empty), byte count, TAG_End
        if stack.is_empty() {
            write.write_i16_be(-1)
        } else {
            write.write_i16_be(item_id as i16)?;
            write.write_i8(stack.item_count as i8)?;
            write.write_u8(0)?; // TAG_End (no NBT)
            Ok(())
        }
    } else {
        // <= 1.12.2: short id (-1 if empty), byte count, short damage, TAG_End
        if stack.is_empty() {
            write.write_i16_be(-1)
        } else {
            write.write_i16_be(item_id as i16)?;
            write.write_i8(stack.item_count as i8)?;
            write.write_i16_be(0)?; // damage / metadata
            write.write_u8(0)?; // TAG_End (no NBT)
            Ok(())
        }
    }
}

fn serialize_length_prefixed_item_stack_with_id(
    stack: &ItemStack,
    item_id: u16,
    version: JavaMinecraftVersion,
    write: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    if version >= JavaMinecraftVersion::V_1_20_5 {
        if stack.is_empty() {
            write.put_var_int(&VarInt(0))
        } else {
            let effective = remapped_patch(stack, version);
            let (to_add, to_remove) =
                effective
                    .iter()
                    .fold((0, 0), |(add, remove), (_, _, data)| {
                        if data.is_some() {
                            (add + 1, remove)
                        } else {
                            (add, remove + 1)
                        }
                    });
            write.put_var_int(&VarInt::from(stack.item_count))?;
            write.put_var_int(&VarInt::from(item_id))?;
            write.put_var_int(&VarInt(to_add))?;
            write.put_var_int(&VarInt(to_remove))?;

            for (wire_id, id, data) in &effective {
                if let Some(data) = data {
                    write.put_var_int(&VarInt(*wire_id as i32))?;
                    let mut comp_buf = Vec::new();
                    serialize(*id, *data, &mut comp_buf)?;
                    write.put_var_int(&VarInt::from(comp_buf.len() as i32))?;
                    write.write_slice(&comp_buf)?;
                }
            }

            for (wire_id, _, data) in &effective {
                if data.is_none() {
                    write.put_var_int(&VarInt(*wire_id as i32))?;
                }
            }

            Ok(())
        }
    } else {
        serialize_item_stack_with_id(stack, item_id, version, write)
    }
}

fn serialize_item_cost_with_id(
    stack: &ItemStack,
    item_id: u16,
    version: JavaMinecraftVersion,
    write: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    let effective = remapped_patch(stack, version);
    let component_count = effective
        .iter()
        .filter(|(_, _, data)| data.is_some())
        .count();
    let component_count = i32::try_from(component_count)
        .map_err(|_| WritingError::Message("Too many item cost components".into()))?;

    write.put_var_int(&VarInt::from(item_id))?;
    write.put_var_int(&VarInt::from(stack.item_count))?;
    write.put_var_int(&VarInt(component_count))?;
    for (wire_id, id, data) in effective {
        if let Some(data) = data {
            write.put_var_int(&VarInt(wire_id as i32))?;
            serialize(id, data, write)?;
        }
    }
    Ok(())
}

fn read_component_id_for_version(
    read: &mut impl NetworkReadExt,
    version: Option<JavaMinecraftVersion>,
) -> Result<DataComponent, ReadingError> {
    let id_val = read.get_var_int()?.0;
    let raw_id = u32::try_from(id_val)
        .map_err(|_| ReadingError::Message(format!("Invalid component ID: {id_val}")))?;
    let remapped_id = version.map_or(raw_id, |version| {
        remap_data_component_type_id_from_version(raw_id, version)
    });
    if raw_id != 0 && remapped_id == 0 {
        return Err(ReadingError::Message(format!(
            "Unsupported component ID for version: {id_val}"
        )));
    }
    let id = u8::try_from(remapped_id)
        .ok()
        .and_then(DataComponent::try_from_id)
        .ok_or_else(|| ReadingError::Message(format!("Unknown component ID: {id_val}")))?;
    Ok(id)
}

fn decode_custom_name(component_data: &[u8]) -> Result<Box<dyn DataComponentImpl>, ReadingError> {
    let mut cursor = Cursor::new(component_data);
    let mut nbt_reader = pumpkin_nbt::deserializer::NbtReadHelperJava::new(&mut cursor);
    let tag = NbtTag::deserialize(&mut nbt_reader)
        .map_err(|err| ReadingError::Message(format!("Failed to decode CustomName NBT: {err}")))?;
    let name = TextComponent::from_nbt(&tag);
    Ok(CustomNameImpl { name }.to_dyn())
}

fn decode_item_name(component_data: &[u8]) -> Result<Box<dyn DataComponentImpl>, ReadingError> {
    let mut cursor = Cursor::new(component_data);
    let mut nbt_reader = pumpkin_nbt::deserializer::NbtReadHelperJava::new(&mut cursor);
    let tag = NbtTag::deserialize(&mut nbt_reader)
        .map_err(|err| ReadingError::Message(format!("Failed to decode ItemName NBT: {err}")))?;
    let name = match tag {
        NbtTag::String(name) => name.to_string(),
        NbtTag::Compound(compound) => compound
            .get_string("translate")
            .or_else(|| compound.get_string("text"))
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    };
    Ok(ItemNameImpl {
        name: Cow::Owned(name),
    }
    .to_dyn())
}

fn decode_custom_data(component_data: &[u8]) -> Result<Box<dyn DataComponentImpl>, ReadingError> {
    let mut cursor = Cursor::new(component_data);
    let mut nbt_reader = pumpkin_nbt::deserializer::NbtReadHelperJava::new(&mut cursor);
    let tag = NbtTag::deserialize(&mut nbt_reader)
        .map_err(|err| ReadingError::Message(format!("Failed to decode CustomData NBT: {err}")))?;
    let data = match tag {
        NbtTag::Compound(compound) => compound,
        _ => pumpkin_nbt::compound::NbtCompound::new(),
    };
    Ok(CustomDataImpl::new(data).to_dyn())
}

fn decode_component(
    id: DataComponent,
    component_data: &[u8],
) -> Result<Box<dyn DataComponentImpl>, ReadingError> {
    match id {
        DataComponent::CustomName => decode_custom_name(component_data),
        DataComponent::ItemName => decode_item_name(component_data),
        DataComponent::CustomData => decode_custom_data(component_data),
        _ => {
            let mut cursor = Cursor::new(component_data);
            deserialize(id, &mut cursor)
        }
    }
}

fn read_length_prefixed_component(
    read: &mut impl NetworkReadExt,
    version: Option<JavaMinecraftVersion>,
) -> Result<(DataComponent, Box<dyn DataComponentImpl>), ReadingError> {
    let id = read_component_id_for_version(read, version)?;
    let byte_len = read.get_var_int()?.0;
    let byte_len: usize = byte_len
        .try_into()
        .map_err(|_| ReadingError::Message("Negative component data length".into()))?;
    if byte_len > crate::MAX_PACKET_DATA_SIZE {
        return Err(ReadingError::TooLarge("Component data too large".into()));
    }

    let component_impl = if byte_len <= 256 {
        let mut stack_buf = [0u8; 256];
        let slice = &mut stack_buf[..byte_len];
        read.read_bytes_to_buf(slice)?;
        decode_component(id, slice)?
    } else {
        let mut component_data = vec![0u8; byte_len];
        read.read_bytes_to_buf(&mut component_data)?;
        decode_component(id, &component_data)?
    };

    Ok((id, component_impl))
}

impl ItemStackSerializer<'_> {
    pub fn read(
        read: &mut impl NetworkReadExt,
    ) -> Result<ItemStackSerializer<'static>, ReadingError> {
        const MAX_COMPONENTS: i32 = 256;

        let item_count = read.get_var_int()?;
        if item_count.0 == 0 {
            return Ok(ItemStackSerializer(Cow::Borrowed(ItemStack::EMPTY)));
        }
        let item_count_u8: u8 = item_count
            .0
            .try_into()
            .map_err(|_| ReadingError::Message("Invalid item count!".into()))?;

        let item_id = read.get_var_int()?;
        let num_to_add = read.get_var_int()?.0;
        let num_to_remove = read.get_var_int()?.0;

        if num_to_add < 0 || num_to_remove < 0 {
            return Err(ReadingError::Message("Negative component count".into()));
        }

        let total_components = num_to_add
            .checked_add(num_to_remove)
            .ok_or_else(|| ReadingError::Message("Component count overflow".into()))?;

        if total_components > MAX_COMPONENTS {
            return Err(ReadingError::Message(
                "Too many components in ItemStack patch".into(),
            ));
        }

        let mut patch = Vec::with_capacity((num_to_add + num_to_remove) as usize);

        for _ in 0..num_to_add {
            let id_val = read.get_var_int()?.0;
            let id = u8::try_from(id_val)
                .ok()
                .and_then(DataComponent::try_from_id)
                .ok_or_else(|| ReadingError::Message(format!("Unknown component ID: {id_val}")))?;

            let component_impl = if id == DataComponent::CustomData {
                CustomDataImpl::deserialize(read)?.to_dyn()
            } else {
                deserialize(id, read)?
            };
            set_patch_entry(&mut patch, id, Some(component_impl));
        }

        for _ in 0..num_to_remove {
            let id_val = read.get_var_int()?.0;
            let id = u8::try_from(id_val)
                .ok()
                .and_then(DataComponent::try_from_id)
                .ok_or_else(|| ReadingError::Message("Unknown component ID".into()))?;
            set_patch_entry(&mut patch, id, None);
        }

        let item_id_u16: u16 = item_id
            .0
            .try_into()
            .map_err(|_| ReadingError::Message("Invalid item id!".into()))?;

        Ok(ItemStackSerializer(Cow::Owned(
            ItemStack::new_with_component(
                item_count_u8,
                Item::from_id(item_id_u16).unwrap_or(&Item::AIR),
                patch,
            ),
        )))
    }

    fn read_modern(
        read: &mut impl NetworkReadExt,
        version: Option<JavaMinecraftVersion>,
    ) -> Result<ItemStackSerializer<'static>, ReadingError> {
        const MAX_COMPONENTS: i32 = 256;

        let item_count = read.get_var_int()?;
        if item_count.0 == 0 {
            return Ok(ItemStackSerializer(Cow::Borrowed(ItemStack::EMPTY)));
        }
        let item_count_u8: u8 = item_count
            .0
            .try_into()
            .map_err(|_| ReadingError::Message("Invalid item count!".into()))?;

        let raw_item_id = read.get_var_int()?;
        let item_id = remap_item_id_from_version(
            raw_item_id
                .0
                .try_into()
                .map_err(|_| ReadingError::Message("Invalid item id!".into()))?,
            version.unwrap_or(JavaMinecraftVersion::V_26_2),
        );
        let item = Item::from_id(item_id).unwrap_or(&Item::AIR);
        let num_to_add = read.get_var_int()?.0;
        let num_to_remove = read.get_var_int()?.0;

        if num_to_add < 0 || num_to_remove < 0 {
            return Err(ReadingError::Message("Negative component count".into()));
        }

        let total_components = num_to_add
            .checked_add(num_to_remove)
            .ok_or_else(|| ReadingError::Message("Component count overflow".into()))?;

        if total_components > MAX_COMPONENTS {
            return Err(ReadingError::Message(
                "Too many components in ItemStack patch".into(),
            ));
        }

        let mut patch = Vec::with_capacity(total_components as usize);
        for _ in 0..num_to_add {
            let id = read_component_id_for_version(read, version)?;
            let component_impl = if id == DataComponent::CustomData {
                CustomDataImpl::deserialize(read)?.to_dyn()
            } else {
                deserialize(id, read)?
            };
            set_patch_entry(&mut patch, id, Some(component_impl));
        }
        for _ in 0..num_to_remove {
            set_patch_entry(
                &mut patch,
                read_component_id_for_version(read, version)?,
                None,
            );
        }

        Ok(ItemStackSerializer(Cow::Owned(
            ItemStack::new_with_component(item_count_u8, item, patch),
        )))
    }

    pub fn read_with_version(
        read: &mut impl NetworkReadExt,
        version: &JavaMinecraftVersion,
    ) -> Result<ItemStackSerializer<'static>, ReadingError> {
        if *version >= JavaMinecraftVersion::V_1_20_5 {
            Self::read_modern(read, Some(*version))
        } else if *version >= JavaMinecraftVersion::V_1_13_2 {
            let present = read.get_bool()?;
            if !present {
                return Ok(ItemStackSerializer(Cow::Borrowed(ItemStack::EMPTY)));
            }
            let raw_item_id = read.get_var_int()?.0 as u16;
            let count = read.get_i8()? as u8;
            let nbt_type = read.get_u8()?;
            if nbt_type != 0 {
                // TAG_End is 0 when no NBT is present
            }
            let item_id = remap_item_id_from_version(raw_item_id, *version);
            let item = Item::from_id(item_id).unwrap_or(&Item::AIR);
            Ok(ItemStackSerializer(Cow::Owned(ItemStack::new(count, item))))
        } else if *version >= JavaMinecraftVersion::V_1_13 {
            let raw_item_id = read.get_i16_be()?;
            if raw_item_id == -1 || raw_item_id < 0 {
                return Ok(ItemStackSerializer(Cow::Borrowed(ItemStack::EMPTY)));
            }
            let count = read.get_i8()? as u8;
            let nbt_type = read.get_u8()?;
            if nbt_type != 0 {
                // TAG_End is 0 when no NBT is present
            }
            let item_id = remap_item_id_from_version(raw_item_id as u16, *version);
            let item = Item::from_id(item_id).unwrap_or(&Item::AIR);
            Ok(ItemStackSerializer(Cow::Owned(ItemStack::new(count, item))))
        } else {
            let raw_item_id = read.get_i16_be()?;
            if raw_item_id == -1 || raw_item_id < 0 {
                return Ok(ItemStackSerializer(Cow::Borrowed(ItemStack::EMPTY)));
            }
            let count = read.get_i8()? as u8;
            let _damage = read.get_i16_be()?;
            let nbt_type = read.get_u8()?;
            if nbt_type != 0 {
                // TAG_End is 0 when no NBT is present
            }
            let item_id = remap_item_id_from_version(raw_item_id as u16, *version);
            let item = Item::from_id(item_id).unwrap_or(&Item::AIR);
            Ok(ItemStackSerializer(Cow::Owned(ItemStack::new(count, item))))
        }
    }

    pub fn read_untrusted_with_version(
        read: &mut impl NetworkReadExt,
        version: &JavaMinecraftVersion,
    ) -> Result<ItemStackSerializer<'static>, ReadingError> {
        if *version >= JavaMinecraftVersion::V_1_21_5 {
            Self::read_length_prefixed_optional_with_version(read, Some(*version))
        } else {
            Self::read_with_version(read, version)
        }
    }

    pub fn read_template_with_version(
        read: &mut impl NetworkReadExt,
        version: &JavaMinecraftVersion,
    ) -> Result<ItemStackSerializer<'static>, ReadingError> {
        if *version < JavaMinecraftVersion::V_26_1 {
            Self::read_with_version(read, version)
        } else {
            Self::read_template0(read, version)
        }
    }

    pub fn read_optional_template_with_version(
        read: &mut impl NetworkReadExt,
        version: &JavaMinecraftVersion,
    ) -> Result<ItemStackSerializer<'static>, ReadingError> {
        if *version < JavaMinecraftVersion::V_26_1 {
            Self::read_with_version(read, version)
        } else if read.get_bool()? {
            Self::read_template0(read, version)
        } else {
            Ok(ItemStackSerializer(Cow::Borrowed(ItemStack::EMPTY)))
        }
    }

    pub fn read_template0(
        read: &mut impl NetworkReadExt,
        version: &JavaMinecraftVersion,
    ) -> Result<ItemStackSerializer<'static>, ReadingError> {
        const MAX_COMPONENTS: i32 = 256;

        let raw_item_id = read.get_var_int()?;
        let item_count = read.get_var_int()?;

        let item_id_u16: u16 = raw_item_id
            .0
            .try_into()
            .map_err(|_| ReadingError::Message("Invalid item id!".into()))?;
        let item_id = remap_item_id_from_version(item_id_u16, *version);
        let item = Item::from_id(item_id).unwrap_or(&Item::AIR);

        let num_to_add = read.get_var_int()?.0;
        let num_to_remove = read.get_var_int()?.0;

        if num_to_add < 0 || num_to_remove < 0 {
            return Err(ReadingError::Message("Negative component count".into()));
        }

        let total_components = num_to_add
            .checked_add(num_to_remove)
            .ok_or_else(|| ReadingError::Message("Component count overflow".into()))?;

        if total_components > MAX_COMPONENTS {
            return Err(ReadingError::Message(
                "Too many components in ItemStack patch".into(),
            ));
        }

        let mut patch = Vec::with_capacity(total_components as usize);

        for _ in 0..num_to_add {
            let id_val = read.get_var_int()?.0;
            let raw_comp_id = u32::try_from(id_val)
                .map_err(|_| ReadingError::Message(format!("Invalid component ID: {id_val}")))?;
            let remapped_comp_id = remap_data_component_type_id_from_version(raw_comp_id, *version);
            if raw_comp_id != 0 && remapped_comp_id == 0 {
                return Err(ReadingError::Message(format!(
                    "Unsupported component ID for version: {id_val}"
                )));
            }
            let id = u8::try_from(remapped_comp_id)
                .ok()
                .and_then(DataComponent::try_from_id)
                .ok_or_else(|| ReadingError::Message(format!("Unknown component ID: {id_val}")))?;

            let component_impl = if id == DataComponent::CustomData {
                CustomDataImpl::deserialize(read)?.to_dyn()
            } else {
                deserialize(id, read)?
            };
            set_patch_entry(&mut patch, id, Some(component_impl));
        }

        for _ in 0..num_to_remove {
            let id_val = read.get_var_int()?.0;
            let raw_comp_id = u32::try_from(id_val)
                .map_err(|_| ReadingError::Message(format!("Invalid component ID: {id_val}")))?;
            let remapped_comp_id = remap_data_component_type_id_from_version(raw_comp_id, *version);
            if raw_comp_id != 0 && remapped_comp_id == 0 {
                return Err(ReadingError::Message(format!(
                    "Unsupported component ID for version: {id_val}"
                )));
            }
            let id = u8::try_from(remapped_comp_id)
                .ok()
                .and_then(DataComponent::try_from_id)
                .ok_or_else(|| ReadingError::Message("Unknown component ID".into()))?;
            set_patch_entry(&mut patch, id, None);
        }

        let item_count_u8: u8 = item_count
            .0
            .try_into()
            .map_err(|_| ReadingError::Message("Invalid item count!".into()))?;

        let stack = ItemStack::new_with_component(item_count_u8, item, patch);
        if stack.is_empty() {
            return Err(ReadingError::Message(
                "Can't read empty item stack template".into(),
            ));
        }

        Ok(ItemStackSerializer(Cow::Owned(stack)))
    }

    pub fn write(&self, write: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        self.write_with_version(write, &JavaMinecraftVersion::V_26_2)
    }

    fn read_length_prefixed_optional_with_version(
        read: &mut impl NetworkReadExt,
        version: Option<JavaMinecraftVersion>,
    ) -> Result<ItemStackSerializer<'static>, ReadingError> {
        const MAX_COMPONENTS: i32 = 256;

        let item_count = read.get_var_int()?;
        if item_count.0 == 0 {
            return Ok(ItemStackSerializer(Cow::Borrowed(ItemStack::EMPTY)));
        }
        let item_count_u8 = item_count
            .0
            .try_into()
            .map_err(|_| ReadingError::Message("Invalid item count!".into()))?;

        let item_id = read.get_var_int()?;
        let num_to_add = read.get_var_int()?.0;
        let num_to_remove = read.get_var_int()?.0;

        if num_to_add < 0 || num_to_remove < 0 {
            return Err(ReadingError::Message("Negative component count".into()));
        }

        let total_components = num_to_add
            .checked_add(num_to_remove)
            .ok_or_else(|| ReadingError::Message("Component count overflow".into()))?;

        if total_components > MAX_COMPONENTS {
            return Err(ReadingError::Message(
                "Too many components in ItemStack patch".into(),
            ));
        }

        let mut patch = Vec::with_capacity(total_components as usize);

        for _ in 0..num_to_add {
            let (id, component_impl) = read_length_prefixed_component(read, version)?;
            set_patch_entry(&mut patch, id, Some(component_impl));
        }

        for _ in 0..num_to_remove {
            set_patch_entry(
                &mut patch,
                read_component_id_for_version(read, version)?,
                None,
            );
        }

        let item_id_u16 = item_id
            .0
            .try_into()
            .map_err(|_| ReadingError::Message("Invalid item id!".into()))?;
        let item_id = remap_item_id_from_version(
            item_id_u16,
            version.unwrap_or(JavaMinecraftVersion::V_26_2),
        );

        Ok(ItemStackSerializer(Cow::Owned(
            ItemStack::new_with_component(
                item_count_u8,
                Item::from_id(item_id).unwrap_or(&Item::AIR),
                patch,
            ),
        )))
    }

    pub fn read_length_prefixed_optional(
        read: &mut impl NetworkReadExt,
    ) -> Result<ItemStackSerializer<'static>, ReadingError> {
        Self::read_length_prefixed_optional_with_version(read, None)
    }

    pub fn write_with_version(
        &self,
        write: &mut impl NetworkWriteExt,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        let remapped_item_id = remap_item_id_for_version(self.0.item.id, *version);
        serialize_item_stack_with_id(self.0.as_ref(), remapped_item_id, *version, write)
    }

    pub fn write_length_prefixed_with_version(
        &self,
        write: &mut impl NetworkWriteExt,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        let remapped_item_id = remap_item_id_for_version(self.0.item.id, *version);
        serialize_length_prefixed_item_stack_with_id(
            self.0.as_ref(),
            remapped_item_id,
            *version,
            write,
        )
    }

    pub fn write_item_cost_with_version(
        &self,
        write: &mut impl NetworkWriteExt,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        let remapped_item_id = remap_item_id_for_version(self.0.item.id, *version);
        serialize_item_cost_with_id(self.0.as_ref(), remapped_item_id, *version, write)
    }

    pub fn write_untrusted_with_version(
        &self,
        write: &mut impl NetworkWriteExt,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        if *version >= JavaMinecraftVersion::V_1_21_5 {
            self.write_length_prefixed_with_version(write, version)
        } else {
            self.write_with_version(write, version)
        }
    }

    pub fn write_template_with_version(
        &self,
        write: &mut impl NetworkWriteExt,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        if *version < JavaMinecraftVersion::V_26_1 {
            self.write_with_version(write, version)
        } else {
            self.write_template0(write, version)
        }
    }

    pub fn write_optional_template_with_version(
        &self,
        write: &mut impl NetworkWriteExt,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        if *version < JavaMinecraftVersion::V_26_1 {
            self.write_with_version(write, version)
        } else if !self.0.is_empty() {
            write.write_bool(true)?;
            self.write_template0(write, version)
        } else {
            write.write_bool(false)
        }
    }

    pub fn write_template0(
        &self,
        write: &mut impl NetworkWriteExt,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        if self.0.is_empty() {
            return Err(WritingError::Message(
                "Can't write empty item stack template".into(),
            ));
        }
        let remapped_item_id = remap_item_id_for_version(self.0.item.id, *version);
        let effective = remapped_patch(self.0.as_ref(), *version);
        let (to_add, to_remove) = effective
            .iter()
            .fold((0, 0), |(add, remove), (_, _, data)| {
                if data.is_some() {
                    (add + 1, remove)
                } else {
                    (add, remove + 1)
                }
            });
        write.put_var_int(&VarInt::from(remapped_item_id))?;
        write.put_var_int(&VarInt::from(self.0.item_count))?;
        write.put_var_int(&VarInt(to_add))?;
        write.put_var_int(&VarInt(to_remove))?;

        for (wire_id, id, data) in &effective {
            if let Some(data) = data {
                write.put_var_int(&VarInt(*wire_id as i32))?;
                serialize(*id, *data, write)?;
            }
        }

        for (wire_id, _, data) in &effective {
            if data.is_none() {
                write.put_var_int(&VarInt(*wire_id as i32))?;
            }
        }

        Ok(())
    }

    #[must_use]
    pub fn to_stack(self) -> ItemStack {
        self.0.into_owned()
    }

    #[must_use]
    pub fn to_stack_for_version(self, version: &JavaMinecraftVersion) -> ItemStack {
        let mut stack = self.0.into_owned();
        if stack.is_empty() {
            return stack;
        }

        let remapped_item_id = remap_item_id_from_version(stack.item.id, *version);
        stack.item = Item::from_id(remapped_item_id).unwrap_or(&Item::AIR);

        let mut patch = Vec::with_capacity(stack.patch.len());
        for (comp_id, comp_data) in stack.patch {
            let remapped_comp_id =
                remap_data_component_type_id_from_version(u32::from(comp_id.to_id()), *version);
            if remapped_comp_id == 0 && comp_id != DataComponent::CustomData {
                continue;
            }
            if let Some(target_comp) = u8::try_from(remapped_comp_id)
                .ok()
                .and_then(DataComponent::try_from_id)
            {
                set_patch_entry(&mut patch, target_comp, comp_data);
            }
        }
        stack.patch = patch;

        stack
    }
}

impl From<ItemStack> for ItemStackSerializer<'_> {
    fn from(item: ItemStack) -> Self {
        ItemStackSerializer(Cow::Owned(item))
    }
}

impl From<Option<ItemStack>> for ItemStackSerializer<'_> {
    fn from(item: Option<ItemStack>) -> Self {
        item.map_or_else(
            || ItemStackSerializer(Cow::Borrowed(ItemStack::EMPTY)),
            ItemStackSerializer::from,
        )
    }
}

#[derive(Debug, Clone)]
pub struct ItemComponentHash {
    pub added: Vec<(VarInt, i32)>,
    pub removed: Vec<VarInt>,
}

impl ItemComponentHash {
    pub fn read(read: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        const MAX_COMPONENTS: i32 = 256;

        let added_length = read.get_var_int()?;
        if added_length.0 < 0 || added_length.0 > MAX_COMPONENTS {
            return Err(ReadingError::Message("added_length out of bounds".into()));
        }
        let mut added = Vec::with_capacity(added_length.0 as usize);
        for _ in 0..added_length.0 {
            let component_id = read.get_var_int()?;
            let component_value = read.get_i32()?;
            added.push((component_id, component_value));
        }

        let removed_length = read.get_var_int()?;
        if removed_length.0 < 0 || removed_length.0 > MAX_COMPONENTS {
            return Err(ReadingError::Message("removed_length out of bounds".into()));
        }
        let mut removed = Vec::with_capacity(removed_length.0 as usize);
        for _ in 0..removed_length.0 {
            let component_id = read.get_var_int()?;
            removed.push(component_id);
        }

        Ok(Self { added, removed })
    }

    pub fn write(&self, write: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        write.put_var_int(&VarInt::from(self.added.len() as i32))?;
        for (id, val) in &self.added {
            write.put_var_int(id)?;
            write.put_i32(*val)?;
        }
        write.put_var_int(&VarInt::from(self.removed.len() as i32))?;
        for id in &self.removed {
            write.put_var_int(id)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ItemStackHash {
    item_id: VarInt,
    count: VarInt,
    components: ItemComponentHash,
}

#[derive(Debug, Clone)]
pub struct OptionalItemStackHash(pub Option<ItemStackHash>);

impl OptionalItemStackHash {
    pub fn read(read: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let is_some = read.get_bool()?;
        if is_some {
            let item_id = read.get_var_int()?;
            let count = read.get_var_int()?;
            let components = ItemComponentHash::read(read)?;

            Ok(Self(Some(ItemStackHash {
                item_id,
                count,
                components,
            })))
        } else {
            Ok(Self(None))
        }
    }

    pub fn write(&self, write: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        if let Some(hash) = &self.0 {
            write.put_bool(true)?;
            write.put_var_int(&hash.item_id)?;
            write.put_var_int(&hash.count)?;
            hash.components.write(write)?;
        } else {
            write.put_bool(false)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn hash_equals(&self, other: &ItemStack) -> bool {
        if let Some(hash) = &self.0 {
            if hash.item_id != other.item.id.into() || hash.count != other.item_count.into() {
                return false;
            }
            let effective = other.effective_patch();
            let (to_add, to_remove) = effective.iter().fold((0, 0), |(add, remove), (_, data)| {
                if data.is_some() {
                    (add + 1, remove)
                } else {
                    (add, remove + 1)
                }
            });
            if to_add != hash.components.added.len() || to_remove != hash.components.removed.len() {
                return false;
            }
            effective.into_iter().all(|(other_id, data)| {
                data.map_or_else(
                    || {
                        hash.components
                            .removed
                            .contains(&VarInt::from(other_id.to_id()))
                    },
                    |data| {
                        hash.components.added.iter().any(|(id, value)| {
                            id == &VarInt::from(other_id.to_id()) && *value == data.get_hash()
                        })
                    },
                )
            })
        } else {
            other.is_empty()
        }
    }
}

pub struct ItemStackTemplateSerializer<'a>(pub Cow<'a, ItemStack>);

impl ItemStackTemplateSerializer<'_> {
    pub fn write_with_version(
        &self,
        write: &mut impl NetworkWriteExt,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        let serializer = ItemStackSerializer(Cow::Borrowed(self.0.as_ref()));
        serializer.write_template_with_version(write, version)
    }

    pub fn write(&self, write: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        self.write_with_version(write, &JavaMinecraftVersion::V_26_2)
    }
}

impl From<ItemStack> for ItemStackTemplateSerializer<'_> {
    fn from(item: ItemStack) -> Self {
        ItemStackTemplateSerializer(Cow::Owned(item))
    }
}

pub struct ItemStackOptionalTemplateSerializer<'a>(pub Cow<'a, ItemStack>);

impl ItemStackOptionalTemplateSerializer<'_> {
    pub fn write_with_version(
        &self,
        write: &mut impl NetworkWriteExt,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        let serializer = ItemStackSerializer(Cow::Borrowed(self.0.as_ref()));
        serializer.write_optional_template_with_version(write, version)
    }

    pub fn write(&self, write: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        self.write_with_version(write, &JavaMinecraftVersion::V_26_2)
    }
}

impl From<ItemStack> for ItemStackOptionalTemplateSerializer<'_> {
    fn from(item: ItemStack) -> Self {
        ItemStackOptionalTemplateSerializer(Cow::Owned(item))
    }
}

impl From<Option<ItemStack>> for ItemStackOptionalTemplateSerializer<'_> {
    fn from(item: Option<ItemStack>) -> Self {
        item.map_or_else(
            || ItemStackOptionalTemplateSerializer(Cow::Borrowed(ItemStack::EMPTY)),
            ItemStackOptionalTemplateSerializer::from,
        )
    }
}

#[cfg(test)]
mod duplicate_component_tests {
    use super::*;
    use pumpkin_data::data_component_impl::{DamageImpl, MaxDamageImpl};
    use std::io::Cursor;

    const ORDINARY_DUPLICATES: &[u8] = &[
        0x01, 0x98, 0x07, 0x03, 0x02, 0x03, 0x01, 0x03, 0x02, 0x02, 0x05, 0x03, 0x03, 0x7f,
    ];
    const ORDINARY_CANONICAL: &[u8] = &[0x01, 0x98, 0x07, 0x01, 0x01, 0x02, 0x05, 0x03, 0x7f];
    const TEMPLATE_DUPLICATES: &[u8] = &[
        0x98, 0x07, 0x01, 0x03, 0x02, 0x03, 0x01, 0x03, 0x02, 0x02, 0x05, 0x03, 0x03, 0x7f,
    ];
    const TEMPLATE_CANONICAL: &[u8] = &[0x98, 0x07, 0x01, 0x01, 0x01, 0x02, 0x05, 0x03, 0x7f];
    const LENGTH_DUPLICATES: &[u8] = &[
        0x01, 0x98, 0x07, 0x03, 0x02, 0x03, 0x01, 0x01, 0x03, 0x01, 0x02, 0x02, 0x01, 0x05, 0x03,
        0x03, 0x7f,
    ];
    const LENGTH_CANONICAL: &[u8] = &[0x01, 0x98, 0x07, 0x01, 0x01, 0x02, 0x01, 0x05, 0x03, 0x7f];

    fn assert_effective(stack: &ItemStack) {
        assert!(!stack.has_data_component(DataComponent::Damage));
        assert_eq!(
            stack
                .get_data_component::<MaxDamageImpl>()
                .map(|c| c.max_damage),
            Some(5)
        );
    }

    #[test]
    fn duplicate_readers_are_lww_and_leave_sentinel() {
        let mut ordinary = Cursor::new(ORDINARY_DUPLICATES);
        let stack = ItemStackSerializer::read(&mut ordinary).unwrap().to_stack();
        assert_effective(&stack);
        assert_eq!(ordinary.get_ref().len() as u64 - ordinary.position(), 1);
        assert_eq!(ordinary.get_ref()[ordinary.position() as usize], 0x7f);

        let mut template = Cursor::new(TEMPLATE_DUPLICATES);
        let stack =
            ItemStackSerializer::read_template0(&mut template, &JavaMinecraftVersion::V_26_2)
                .unwrap()
                .to_stack();
        assert_effective(&stack);
        assert_eq!(template.get_ref().len() as u64 - template.position(), 1);
        assert_eq!(template.get_ref()[template.position() as usize], 0x7f);

        let mut length = Cursor::new(LENGTH_DUPLICATES);
        let stack = ItemStackSerializer::read_length_prefixed_optional(&mut length)
            .unwrap()
            .to_stack();
        assert_effective(&stack);
        assert_eq!(length.get_ref().len() as u64 - length.position(), 1);
        assert_eq!(length.get_ref()[length.position() as usize], 0x7f);
    }

    #[test]
    fn duplicate_writers_use_one_canonical_patch_view() {
        let mut input = Cursor::new(ORDINARY_DUPLICATES);
        let stack = ItemStackSerializer::read(&mut input).unwrap().to_stack();
        let serializer = ItemStackSerializer::from(stack);

        let mut output = Vec::new();
        serializer.write(&mut output).unwrap();
        output.push(0x7f);
        assert_eq!(output, ORDINARY_CANONICAL);

        let mut output = Vec::new();
        serializer
            .write_template_with_version(&mut output, &JavaMinecraftVersion::V_26_2)
            .unwrap();
        output.push(0x7f);
        assert_eq!(output, TEMPLATE_CANONICAL);

        let mut output = Vec::new();
        serializer
            .write_length_prefixed_with_version(&mut output, &JavaMinecraftVersion::V_26_2)
            .unwrap();
        output.push(0x7f);
        assert_eq!(output, LENGTH_CANONICAL);
    }

    #[test]
    fn version_775_template_duplicate_fixture_preserves_effective_state() {
        let mut input = Cursor::new(TEMPLATE_DUPLICATES);
        let stack = ItemStackSerializer::read_template0(&mut input, &JavaMinecraftVersion::V_26_1)
            .unwrap()
            .to_stack();
        assert_effective(&stack);
        assert_eq!(input.get_ref().len() as u64 - input.position(), 1);
        assert_eq!(input.get_ref()[input.position() as usize], 0x7f);
    }

    #[test]
    fn malformed_first_duplicate_and_unknown_id_are_rejected() {
        assert!(
            ItemStackSerializer::read(&mut Cursor::new([0x01, 0x98, 0x07, 0x02, 0x00, 0x03, 0x80]))
                .is_err()
        );
        assert!(
            ItemStackSerializer::read(&mut Cursor::new([0x01, 0x98, 0x07, 0x01, 0x00, 0x80, 0x02]))
                .is_err()
        );
    }

    #[test]
    fn public_duplicate_order_uses_first_seen_wire_key_order() {
        let stack = ItemStack::new_with_component(
            1,
            &Item::BOWL,
            vec![
                (
                    DataComponent::Damage,
                    Some(DamageImpl { damage: 1 }.to_dyn()),
                ),
                (
                    DataComponent::MaxDamage,
                    Some(MaxDamageImpl { max_damage: 5 }.to_dyn()),
                ),
                (
                    DataComponent::Damage,
                    Some(DamageImpl { damage: 2 }.to_dyn()),
                ),
            ],
        );
        let mut output = Vec::new();
        ItemStackSerializer::from(stack)
            .write_with_version(&mut output, &JavaMinecraftVersion::V_26_2)
            .unwrap();
        assert_eq!(
            output,
            [0x01, 0x98, 0x07, 0x02, 0x00, 0x03, 0x02, 0x02, 0x05]
        );
    }

    #[test]
    fn versioned_readers_remap_26_1_component_ids_before_codec_dispatch() {
        let mut ordinary = Cursor::new([0x01, 0x98, 0x07, 0x01, 0x00, 0x4e, 0x7f]);
        let stack =
            ItemStackSerializer::read_with_version(&mut ordinary, &JavaMinecraftVersion::V_26_1)
                .unwrap()
                .to_stack();
        assert!(stack.has_data_component(DataComponent::Lock));
        assert_eq!(ordinary.get_ref()[ordinary.position() as usize], 0x7f);

        let mut length = Cursor::new([0x01, 0x98, 0x07, 0x01, 0x00, 0x4e, 0x00, 0x7f]);
        let stack = ItemStackSerializer::read_untrusted_with_version(
            &mut length,
            &JavaMinecraftVersion::V_26_1,
        )
        .unwrap()
        .to_stack();
        assert!(stack.has_data_component(DataComponent::Lock));
        assert_eq!(length.get_ref()[length.position() as usize], 0x7f);

        let mut template = Cursor::new([0x98, 0x07, 0x01, 0x01, 0x00, 0x4e, 0x7f]);
        let stack =
            ItemStackSerializer::read_template0(&mut template, &JavaMinecraftVersion::V_26_1)
                .unwrap()
                .to_stack();
        assert!(stack.has_data_component(DataComponent::Lock));
        assert_eq!(template.get_ref()[template.position() as usize], 0x7f);
    }

    #[test]
    fn versioned_writer_omits_unsupported_component_sentinel_ids() {
        let stack = ItemStack::new_with_component(
            1,
            &Item::BOWL,
            vec![(DataComponent::SulfurCubeContent, None)],
        );
        let mut output = Vec::new();
        ItemStackSerializer::from(stack)
            .write_with_version(&mut output, &JavaMinecraftVersion::V_26_1)
            .unwrap();
        assert_eq!(output, [0x01, 0xfd, 0x06, 0x00, 0x00]);
    }

    #[test]
    fn high_component_ids_are_rejected_by_all_current_patch_readers() {
        assert!(
            ItemStackSerializer::read(&mut Cursor::new(
                [0x01, 0x98, 0x07, 0x00, 0x01, 0x80, 0x02,]
            ))
            .is_err()
        );
        assert!(
            ItemStackSerializer::read_template0(
                &mut Cursor::new([0x98, 0x07, 0x00, 0x01, 0x80, 0x02]),
                &JavaMinecraftVersion::V_26_2,
            )
            .is_err()
        );
        assert!(
            ItemStackSerializer::read_length_prefixed_optional(&mut Cursor::new([
                0x01, 0x98, 0x07, 0x00, 0x01, 0x80, 0x02,
            ]))
            .is_err()
        );
    }

    #[test]
    fn forwarded_hash_uses_the_same_effective_patch_view() {
        let stack = ItemStack::new_with_component(
            1,
            &Item::BOWL,
            vec![
                (
                    DataComponent::Damage,
                    Some(DamageImpl { damage: 1 }.to_dyn()),
                ),
                (
                    DataComponent::Damage,
                    Some(DamageImpl { damage: 2 }.to_dyn()),
                ),
                (
                    DataComponent::MaxDamage,
                    Some(MaxDamageImpl { max_damage: 5 }.to_dyn()),
                ),
                (DataComponent::Damage, None),
            ],
        );
        let hash = OptionalItemStackHash(Some(ItemStackHash {
            item_id: VarInt::from(Item::BOWL.id),
            count: VarInt::from(1),
            components: ItemComponentHash {
                added: vec![(
                    VarInt::from(DataComponent::MaxDamage.to_id()),
                    MaxDamageImpl { max_damage: 5 }.get_hash(),
                )],
                removed: vec![VarInt::from(DataComponent::Damage.to_id())],
            },
        }));
        assert!(hash.hash_equals(&stack));
    }
}
