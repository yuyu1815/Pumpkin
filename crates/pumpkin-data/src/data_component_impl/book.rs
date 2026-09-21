use crate::{Block, data_component_impl::DataComponentImpl};
use crc_fast::CrcAlgorithm::Crc32Iscsi;
use crc_fast::Digest;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_nbt::tag::NbtTag;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct WritableBookContentImpl {
    pub pages: Vec<String>,
}
impl WritableBookContentImpl {
    pub fn read_data(tag: &NbtTag) -> Option<Self> {
        let mut pages = Vec::new();
        if let NbtTag::Compound(c) = tag
            && let Some(NbtTag::List(l)) = c.get("pages")
        {
            for item in l {
                if let NbtTag::String(s) = item {
                    pages.push(s.to_string());
                } else if let NbtTag::Compound(comp) = item {
                    if let Some(s) = comp.get_string("raw") {
                        pages.push(s.to_string());
                    }
                }
            }
        }
        Some(Self { pages })
    }
}
impl DataComponentImpl for WritableBookContentImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        let pages_tags: Vec<NbtTag> = self
            .pages
            .iter()
            .map(|p| NbtTag::String(p.clone().into_boxed_str()))
            .collect();
        compound.put("pages", NbtTag::List(pages_tags));
        NbtTag::Compound(compound)
    }
    default_impl!(WritableBookContent);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct WrittenBookContentImpl {
    pub title: String,
    pub author: String,
    pub pages: Vec<String>,
}
impl WrittenBookContentImpl {
    pub fn read_data(tag: &NbtTag) -> Option<Self> {
        let mut pages = Vec::new();
        let mut title = String::new();
        let mut author = String::new();
        if let NbtTag::Compound(c) = tag {
            if let Some(s) = c.get_string("title") {
                title = s.to_string();
            }
            if let Some(s) = c.get_string("author") {
                author = s.to_string();
            }
            if let Some(NbtTag::List(l)) = c.get("pages") {
                for item in l {
                    if let NbtTag::String(s) = item {
                        pages.push(s.to_string());
                    }
                }
            }
        }
        Some(Self {
            title,
            author,
            pages,
        })
    }
}
impl DataComponentImpl for WrittenBookContentImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        compound.put_string("title", self.title.clone());
        compound.put_string("author", self.author.clone());
        let pages_tags: Vec<NbtTag> = self
            .pages
            .iter()
            .map(|p| NbtTag::String(p.clone().into_boxed_str()))
            .collect();
        compound.put("pages", NbtTag::List(pages_tags));
        NbtTag::Compound(compound)
    }
    default_impl!(WrittenBookContent);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugStickStateImpl {
    pub properties: BTreeMap<String, String>,
}
impl DebugStickStateImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let NbtTag::Compound(compound) = data else {
            return None;
        };
        let mut properties = BTreeMap::new();
        for (block_key, tag) in &compound.child_tags {
            let NbtTag::String(property) = tag else {
                return None;
            };
            let path = block_key.strip_prefix("minecraft:")?;
            let block = Block::from_registry_key(path)?;
            let block_properties = block.properties(block.default_state.id)?;
            if !block_properties
                .to_props()
                .iter()
                .any(|(name, _)| *name == property.as_ref())
            {
                return None;
            }
            properties.insert(format!("minecraft:{}", block.name), property.to_string());
        }
        Some(Self { properties })
    }
}
impl DataComponentImpl for DebugStickStateImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        for (block, property) in &self.properties {
            compound.put_string(block, property.clone());
        }
        NbtTag::Compound(compound)
    }

    fn get_hash(&self) -> i32 {
        let mut digest = Digest::new(Crc32Iscsi);
        for (block, property) in &self.properties {
            digest.update(block.as_bytes());
            digest.update(&[0]);
            digest.update(property.as_bytes());
            digest.update(&[0]);
        }
        digest.finalize() as i32
    }

    default_impl!(DebugStickState);
}
