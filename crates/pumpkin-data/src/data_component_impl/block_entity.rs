use crate::data_component_impl::DataComponentImpl;
use crc_fast::CrcAlgorithm::Crc32Iscsi;
use crc_fast::Digest;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_nbt::tag::NbtTag;
use pumpkin_util::identifier::Identifier;

#[derive(Clone, Debug, PartialEq)]
pub struct BlockEntityDataImpl {
    pub nbt: NbtCompound,
}
impl BlockEntityDataImpl {
    pub fn read_data(tag: &NbtTag) -> Option<Self> {
        if let NbtTag::Compound(c) = tag {
            Some(Self { nbt: c.clone() })
        } else {
            None
        }
    }
}
impl DataComponentImpl for BlockEntityDataImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::Compound(self.nbt.clone())
    }
    default_impl!(BlockEntityData);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct EntityDataImpl;
impl EntityDataImpl {
    pub const fn read_data(_data: &NbtTag) -> Option<Self> {
        Some(Self)
    }
}
impl DataComponentImpl for EntityDataImpl {
    default_impl!(EntityData);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct BucketEntityDataImpl;
impl BucketEntityDataImpl {
    pub const fn read_data(_data: &NbtTag) -> Option<Self> {
        Some(Self)
    }
}
impl DataComponentImpl for BucketEntityDataImpl {
    default_impl!(BucketEntityData);
}

#[derive(Clone)]
pub struct ContainerImpl {
    pub items: Vec<(u8, crate::item_stack::ItemStack)>,
}
impl PartialEq for ContainerImpl {
    fn eq(&self, other: &Self) -> bool {
        self.items.len() == other.items.len()
            && self.items.iter().all(|(slot, stack)| {
                other
                    .items
                    .iter()
                    .find(|(other_slot, _)| other_slot == slot)
                    .is_some_and(|(_, other_stack)| stack.are_equal(other_stack))
            })
    }
}
impl Eq for ContainerImpl {}
impl std::fmt::Debug for ContainerImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ContainerImpl")
    }
}
impl ContainerImpl {
    pub fn read_data(tag: &NbtTag) -> Option<Self> {
        let list = tag.extract_list()?;
        let mut items = Vec::with_capacity(list.len());
        for item_tag in list {
            let compound = item_tag.extract_compound()?;
            let slot = crate::data_component_impl::food::nbt_i32(compound.get("slot")?)?;
            let slot = u8::try_from(slot).ok()?;
            let stack =
                crate::item_stack::ItemStack::read_item_stack_template(compound.get("item")?)?;
            items.push((slot, stack));
        }
        Some(Self { items })
    }
}
impl DataComponentImpl for ContainerImpl {
    fn write_data(&self) -> NbtTag {
        let mut list = Vec::new();
        for (slot, stack) in &self.items {
            let mut entry = NbtCompound::new();
            entry.put_int("slot", *slot as i32);
            let mut item_compound = NbtCompound::new();
            stack.write_item_stack(&mut item_compound);
            entry.put_compound("item", item_compound);
            list.push(NbtTag::Compound(entry));
        }
        NbtTag::List(list)
    }
    fn get_hash(&self) -> i32 {
        let mut slots = self
            .items
            .iter()
            .map(|(slot, stack)| (*slot, stack.get_hash()))
            .collect::<Vec<_>>();
        slots.sort_unstable_by_key(|(slot, _)| *slot);
        let mut digest = Digest::new(Crc32Iscsi);
        for (slot, hash) in slots {
            digest.update(&[slot]);
            digest.update(&hash.to_le_bytes());
        }
        digest.finalize() as i32
    }
    default_impl!(Container);
}

use std::borrow::Cow;

#[derive(Clone, Debug)]
pub struct BlockStateImpl {
    pub properties: Cow<'static, [(Cow<'static, str>, Cow<'static, str>)]>,
}
impl PartialEq for BlockStateImpl {
    fn eq(&self, other: &Self) -> bool {
        let mut self_props = self.properties.to_vec();
        self_props.sort_by(|a, b| a.0.cmp(&b.0));
        let mut other_props = other.properties.to_vec();
        other_props.sort_by(|a, b| a.0.cmp(&b.0));
        self_props == other_props
    }
}
impl Eq for BlockStateImpl {}
impl std::hash::Hash for BlockStateImpl {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let mut props = self.properties.to_vec();
        props.sort_by(|a, b| a.0.cmp(&b.0));
        for (k, v) in props {
            k.hash(state);
            v.hash(state);
        }
    }
}
impl BlockStateImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let compound = data.extract_compound()?;
        let mut properties = Vec::new();
        for (key, val) in compound.child_tags.iter() {
            if let Some(s) = val.extract_string() {
                properties.push((Cow::Owned(key.to_string()), Cow::Owned(s.to_string())));
            }
        }
        Some(Self {
            properties: Cow::Owned(properties),
        })
    }
}
impl DataComponentImpl for BlockStateImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        for (k, v) in self.properties.iter() {
            compound.put_string(k.as_ref(), v.to_string());
        }
        NbtTag::Compound(compound)
    }
    fn get_hash(&self) -> i32 {
        let mut digest = Digest::new(Crc32Iscsi);
        let mut props = self.properties.to_vec();
        props.sort_by(|a, b| a.0.cmp(&b.0));
        for (k, v) in props {
            digest.update(k.as_ref().as_bytes());
            digest.update(v.as_ref().as_bytes());
        }
        digest.finalize() as i32
    }
    default_impl!(BlockState);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct BeesImpl;
impl BeesImpl {
    pub const fn read_data(_data: &NbtTag) -> Option<Self> {
        Some(Self)
    }
}
impl DataComponentImpl for BeesImpl {
    default_impl!(Bees);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ContainerLootImpl {
    pub loot_table: String,
    pub seed: i64,
}
impl ContainerLootImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let compound = data.extract_compound()?;
        let loot_table = Identifier::parse(compound.get_string("loot_table")?)
            .ok()?
            .to_string();
        let seed = compound.get("seed").map_or(Some(0), nbt_long)?;
        Some(Self { loot_table, seed })
    }
}

fn nbt_long(tag: &NbtTag) -> Option<i64> {
    match tag {
        NbtTag::Byte(value) => Some(i64::from(*value)),
        NbtTag::Short(value) => Some(i64::from(*value)),
        NbtTag::Int(value) => Some(i64::from(*value)),
        NbtTag::Long(value) => Some(*value),
        NbtTag::Float(value) => Some(*value as i64),
        NbtTag::Double(value) => Some(*value as i64),
        _ => None,
    }
}
impl DataComponentImpl for ContainerLootImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        compound.put_string("loot_table", self.loot_table.clone());
        if self.seed != 0 {
            compound.put_long("seed", self.seed);
        }
        NbtTag::Compound(compound)
    }
    default_impl!(ContainerLoot);
}

#[derive(Clone)]
pub struct SulfurCubeContentImpl {
    pub absorbed_block_item_stack: crate::item_stack::ItemStack,
}
impl std::fmt::Debug for SulfurCubeContentImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SulfurCubeContentImpl")
            .field("item", &self.absorbed_block_item_stack.item.registry_key)
            .field("count", &self.absorbed_block_item_stack.item_count)
            .finish()
    }
}
impl PartialEq for SulfurCubeContentImpl {
    fn eq(&self, other: &Self) -> bool {
        self.absorbed_block_item_stack
            .are_equal(&other.absorbed_block_item_stack)
    }
}
impl Eq for SulfurCubeContentImpl {}
impl SulfurCubeContentImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        Some(Self {
            absorbed_block_item_stack: crate::item_stack::ItemStack::read_item_stack_template(
                data,
            )?,
        })
    }
}
impl DataComponentImpl for SulfurCubeContentImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        self.absorbed_block_item_stack
            .write_item_stack(&mut compound);
        NbtTag::Compound(compound)
    }
    fn get_hash(&self) -> i32 {
        self.absorbed_block_item_stack.get_hash()
    }
    default_impl!(SulfurCubeContent);
}
