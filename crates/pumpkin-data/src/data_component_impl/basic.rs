use crate::data_component::DataComponent;
use crate::data_component_impl::{DataComponentImpl, get_i32_hash, get_str_hash};
use crc_fast::CrcAlgorithm::Crc32Iscsi;
use crc_fast::Digest;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_nbt::tag::NbtTag;
use pumpkin_util::text::TextComponent;
use std::borrow::Cow;

#[derive(Clone, Debug)]
pub struct CustomDataImpl {
    pub data: NbtCompound,
}

impl PartialEq for CustomDataImpl {
    fn eq(&self, other: &Self) -> bool {
        custom_data_compounds_equal(&self.data, &other.data)
    }
}

fn custom_data_compounds_equal(left: &NbtCompound, right: &NbtCompound) -> bool {
    left.child_tags.len() == right.child_tags.len()
        && left.child_tags.iter().all(|(key, value)| {
            right
                .child_tags
                .get(key)
                .is_some_and(|other| custom_data_tags_equal(value, other))
        })
}

fn custom_data_tags_equal(left: &NbtTag, right: &NbtTag) -> bool {
    match (left, right) {
        (NbtTag::End, NbtTag::End) => true,
        (NbtTag::Byte(left), NbtTag::Byte(right)) => left == right,
        (NbtTag::Short(left), NbtTag::Short(right)) => left == right,
        (NbtTag::Int(left), NbtTag::Int(right)) => left == right,
        (NbtTag::Long(left), NbtTag::Long(right)) => left == right,
        (NbtTag::Float(left), NbtTag::Float(right)) => float_bits(*left) == float_bits(*right),
        (NbtTag::Double(left), NbtTag::Double(right)) => {
            double_bits(*left) == double_bits(*right)
        }
        (NbtTag::ByteArray(left), NbtTag::ByteArray(right)) => left == right,
        (NbtTag::String(left), NbtTag::String(right)) => left == right,
        (NbtTag::List(left), NbtTag::List(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| custom_data_tags_equal(left, right))
        }
        (NbtTag::Compound(left), NbtTag::Compound(right)) => {
            custom_data_compounds_equal(left, right)
        }
        (NbtTag::IntArray(left), NbtTag::IntArray(right)) => left == right,
        (NbtTag::LongArray(left), NbtTag::LongArray(right)) => left == right,
        _ => false,
    }
}

fn float_bits(value: f32) -> u32 {
    if value.is_nan() {
        f32::NAN.to_bits()
    } else {
        value.to_bits()
    }
}

fn double_bits(value: f64) -> u64 {
    if value.is_nan() {
        f64::NAN.to_bits()
    } else {
        value.to_bits()
    }
}

impl CustomDataImpl {
    #[must_use]
    pub const fn new(data: NbtCompound) -> Self {
        Self { data }
    }
    #[must_use]
    pub fn read_data(tag: &NbtTag) -> Option<Self> {
        if let NbtTag::Compound(c) = tag {
            Some(Self { data: c.clone() })
        } else {
            None
        }
    }
}
// Mirrors HashOps.CRC32C_INSTANCE over the values produced by CompoundTag.CODEC/NbtOps.
fn custom_data_hash(tag: &NbtTag) -> u32 {
    if let NbtTag::Compound(compound) = tag {
        return custom_data_hash_compound(compound);
    }
    if let NbtTag::String(value) = tag {
        return custom_data_hash_string(value);
    }
    let mut digest = Digest::new(Crc32Iscsi);
    match tag {
        NbtTag::End => digest.update(&[1]),
        NbtTag::Byte(value) => digest.update(&[6, *value as u8]),
        NbtTag::Short(value) => {
            digest.update(&[7]);
            digest.update(&value.to_le_bytes());
        }
        NbtTag::Int(value) => {
            digest.update(&[8]);
            digest.update(&value.to_le_bytes());
        }
        NbtTag::Long(value) => {
            digest.update(&[9]);
            digest.update(&value.to_le_bytes());
        }
        NbtTag::Float(value) => {
            digest.update(&[10]);
            digest.update(&float_bits(*value).to_le_bytes());
        }
        NbtTag::Double(value) => {
            digest.update(&[11]);
            digest.update(&double_bits(*value).to_le_bytes());
        }
        NbtTag::String(_) => unreachable!("strings are handled above"),
        NbtTag::List(values) => {
            digest.update(&[4]);
            for value in values {
                digest.update(&custom_data_hash(value).to_le_bytes());
            }
            digest.update(&[5]);
        }
        NbtTag::Compound(_) => unreachable!("compounds are handled above"),
        NbtTag::ByteArray(values) => {
            digest.update(&[14]);
            for value in values.iter() {
                digest.update(&[*value as u8]);
            }
            digest.update(&[15]);
        }
        NbtTag::IntArray(values) => {
            digest.update(&[16]);
            for value in values {
                digest.update(&value.to_le_bytes());
            }
            digest.update(&[17]);
        }
        NbtTag::LongArray(values) => {
            digest.update(&[18]);
            for value in values {
                digest.update(&value.to_le_bytes());
            }
            digest.update(&[19]);
        }
    }
    digest.finalize() as u32
}

fn custom_data_hash_string(value: &str) -> u32 {
    let mut digest = Digest::new(Crc32Iscsi);
    digest.update(&[12]);
    let utf16 = value.encode_utf16().collect::<Vec<_>>();
    digest.update(&(utf16.len() as i32).to_le_bytes());
    for unit in utf16 {
        digest.update(&unit.to_le_bytes());
    }
    digest.finalize() as u32
}

fn custom_data_hash_compound(compound: &NbtCompound) -> u32 {
    let mut digest = Digest::new(Crc32Iscsi);
    digest.update(&[2]);
    let mut entries = compound
        .child_tags
        .iter()
        .map(|(key, value)| (custom_data_hash_string(key), custom_data_hash(value)))
        .collect::<Vec<_>>();
    entries.sort_unstable();
    for (key_hash, value_hash) in entries {
        digest.update(&key_hash.to_le_bytes());
        digest.update(&value_hash.to_le_bytes());
    }
    digest.update(&[3]);
    digest.finalize() as u32
}

impl DataComponentImpl for CustomDataImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::Compound(self.data.clone())
    }
    fn get_hash(&self) -> i32 {
        custom_data_hash_compound(&self.data) as i32
    }
    default_impl!(CustomData);
}

#[cfg(test)]
mod custom_data_hash_tests {
    use super::CustomDataImpl;
    use crate::data_component_impl::DataComponentImpl;
    use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};

    fn hash(data: NbtCompound) -> i32 {
        CustomDataImpl::new(data).get_hash()
    }

    #[test]
    fn matches_hashops_crc32c_for_compounds_and_nested_values() {
        assert_eq!(hash(NbtCompound::new()), -982_207_288); // CRC32C([2, 3])

        let mut first = NbtCompound::new();
        first.put(
            "z",
            NbtTag::List(vec![NbtTag::Int(-5), NbtTag::String("🌱".into())]),
        );
        first.put("a", NbtTag::ByteArray(vec![-1, 0, 1].into()));
        let mut reverse = NbtCompound::new();
        reverse.put("a", NbtTag::ByteArray(vec![-1, 0, 1].into()));
        reverse.put(
            "z",
            NbtTag::List(vec![NbtTag::Int(-5), NbtTag::String("🌱".into())]),
        );
        let first = CustomDataImpl::new(first);
        let reverse = CustomDataImpl::new(reverse);
        assert_eq!(first, reverse);
        assert_eq!(first.get_hash(), reverse.get_hash());

        let mut arrays = NbtCompound::new();
        arrays.put("ints", NbtTag::IntArray(vec![-1, 0, 1]));
        arrays.put("longs", NbtTag::LongArray(vec![i64::MIN, 1]));
        assert_ne!(hash(arrays), hash(NbtCompound::new()));
    }

    #[test]
    fn equality_and_hash_distinguish_signed_zero() {
        let mut positive = NbtCompound::new();
        positive.put("f", NbtTag::Double(0.0));
        let mut negative = NbtCompound::new();
        negative.put("f", NbtTag::Double(-0.0));
        let positive = CustomDataImpl::new(positive);
        let negative = CustomDataImpl::new(negative);
        assert_ne!(positive, negative);
        assert_ne!(positive.get_hash(), negative.get_hash());
        assert!(!positive.equal(&negative));
    }

    #[test]
    fn equality_matches_protocol_semantics_for_every_tag_variant() {
        let tags = [
            NbtTag::End,
            NbtTag::Byte(-1),
            NbtTag::Short(-2),
            NbtTag::Int(-3),
            NbtTag::Long(-4),
            NbtTag::Float(-0.0),
            NbtTag::Double(-0.0),
            NbtTag::ByteArray(vec![-1, 0, 1].into()),
            NbtTag::String("value".into()),
            NbtTag::List(vec![NbtTag::Int(1), NbtTag::String("nested".into())]),
            NbtTag::Compound({
                let mut compound = NbtCompound::new();
                compound.put("nested", NbtTag::Byte(1));
                compound
            }),
            NbtTag::IntArray(vec![-1, 0, 1]),
            NbtTag::LongArray(vec![-1, 0, 1]),
        ];
        for (index, tag) in tags.iter().enumerate() {
            let mut left = NbtCompound::new();
            left.put("tag", tag.clone());
            let mut right = NbtCompound::new();
            right.put("tag", tag.clone());
            assert_eq!(
                CustomDataImpl::new(left),
                CustomDataImpl::new(right),
                "tag {index}"
            );
        }
    }

    #[test]
    fn nan_payloads_compare_equal_and_hash_canonically() {
        let mut left = NbtCompound::new();
        left.put("float", NbtTag::Float(f32::from_bits(0x7fc0_0001)));
        left.put("double", NbtTag::Double(f64::from_bits(0x7ff8_0000_0000_0001)));
        let mut right = NbtCompound::new();
        right.put("float", NbtTag::Float(f32::from_bits(0xffc0_1234)));
        right.put("double", NbtTag::Double(f64::from_bits(0xfff8_0000_0000_1234)));
        let left = CustomDataImpl::new(left);
        let right = CustomDataImpl::new(right);
        assert_eq!(left, right);
        assert_eq!(left.get_hash(), right.get_hash());
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct MaxStackSizeImpl {
    pub size: u8,
}
impl MaxStackSizeImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        data.extract_int().map(|size| Self { size: size as u8 })
    }
}
impl DataComponentImpl for MaxStackSizeImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::Int(self.size as i32)
    }
    fn get_hash(&self) -> i32 {
        get_i32_hash(self.size as i32) as i32
    }
    default_impl!(MaxStackSize);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct MaxDamageImpl {
    pub max_damage: i32,
}
impl MaxDamageImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        data.extract_int().map(|max_damage| Self { max_damage })
    }
}
impl DataComponentImpl for MaxDamageImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::Int(self.max_damage)
    }
    default_impl!(MaxDamage);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct DamageImpl {
    pub damage: i32,
}
impl DamageImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        data.extract_int().map(|damage| Self { damage })
    }
}
impl DataComponentImpl for DamageImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::Int(self.damage)
    }
    fn get_hash(&self) -> i32 {
        get_i32_hash(self.damage) as i32
    }
    default_impl!(Damage);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct UnbreakableImpl;
impl UnbreakableImpl {
    pub const fn read_data(_data: &NbtTag) -> Option<Self> {
        Some(Self)
    }
}
impl DataComponentImpl for UnbreakableImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::Compound(NbtCompound::new())
    }
    fn get_hash(&self) -> i32 {
        0
    }
    default_impl!(Unbreakable);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct CustomNameImpl {
    pub name: TextComponent,
}
impl CustomNameImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let name = match data {
            NbtTag::String(name) => TextComponent::text(name.to_string()),
            NbtTag::Compound(_) => TextComponent::from_nbt(data),
            _ => return None,
        };
        Some(Self { name })
    }
}
impl DataComponentImpl for CustomNameImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::String(self.name.clone().get_text().into())
    }
    fn get_hash(&self) -> i32 {
        get_str_hash(self.name.clone().get_text().as_str()) as i32
    }
    default_impl!(CustomName);
}

#[derive(Clone, Debug)]
pub enum ItemName {
    /// Generated vanilla item names are translation keys and must be const-constructible.
    Translation(&'static str),
    /// Runtime-provided names retain their complete text component structure.
    Component(TextComponent),
}

impl PartialEq for ItemName {
    fn eq(&self, other: &Self) -> bool {
        self.as_component() == other.as_component()
    }
}
impl Eq for ItemName {}
impl std::hash::Hash for ItemName {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.as_component(), state);
    }
}

impl ItemName {
    #[must_use]
    pub const fn translated(key: &'static str) -> Self {
        Self::Translation(key)
    }

    #[must_use]
    pub fn as_component(&self) -> TextComponent {
        match self {
            Self::Translation(key) => TextComponent::translate(*key, vec![]),
            Self::Component(component) => component.clone(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ItemNameImpl {
    pub name: ItemName,
}
impl ItemNameImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        if !matches!(data, NbtTag::String(_) | NbtTag::Compound(_) | NbtTag::List(_)) {
            return None;
        }
        Some(Self {
            name: ItemName::Component(TextComponent::try_from_nbt(data).ok()?),
        })
    }
}
impl DataComponentImpl for ItemNameImpl {
    fn write_data(&self) -> NbtTag {
        self.name
            .as_component()
            .to_nbt_tag_for_version(&pumpkin_util::version::JavaMinecraftVersion::V_26_2)
    }
    fn get_hash(&self) -> i32 {
        // Official HashOps parity is unresolved. This fallback satisfies Eq => hash: equal
        // components have the same translation key or visible text; style-only differences may
        // collide, which is permitted.
        match &self.name {
            ItemName::Translation(key) => get_str_hash(key.as_ref()) as i32,
            ItemName::Component(component) => match &*component.0.content {
                pumpkin_util::text::TextContent::Translate { translate, .. } => {
                    get_str_hash(translate.as_ref()) as i32
                }
                _ => get_str_hash(component.clone().get_text().as_str()) as i32,
            },
        }
    }
    default_impl!(ItemName);
}

#[cfg(test)]
mod item_name_tests {
    use super::*;

    #[test]
    fn item_name_nbt_roundtrip_preserves_components_and_parses_lists() {
        let literal = ItemNameImpl {
            name: ItemName::Component(TextComponent::text("item.minecraft.apple")),
        };
        let translated = ItemNameImpl {
            name: ItemName::Component(
                TextComponent::translate("item.minecraft.apple", vec![]).bold(),
            ),
        };
        assert_ne!(literal.name.as_component(), translated.name.as_component());
        // Dynamic fallback is not official parity; the literal/translation collision is valid.
        assert_eq!(literal.get_hash(), translated.get_hash());
        let generated = ItemNameImpl {
            name: ItemName::translated("item.minecraft.apple"),
        };
        assert_eq!(generated.get_hash(), get_str_hash("item.minecraft.apple") as i32);

        for original in [literal, translated] {
            let decoded = ItemNameImpl::read_data(&original.write_data()).expect("valid component");
            assert_eq!(decoded, original);
        }
        assert!(ItemNameImpl::read_data(&NbtTag::Int(1)).is_none());
        let list = NbtTag::List(vec![
            NbtTag::String("first".into()),
            NbtTag::String("second".into()),
        ]);
        assert!(ItemNameImpl::read_data(&list).is_some());
        assert!(ItemNameImpl::read_data(&NbtTag::List(vec![])).is_none());
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ItemModelImpl {
    pub id: Cow<'static, str>,
}
impl ItemModelImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        data.extract_string().map(|id| Self {
            id: Cow::Owned(id.to_string()),
        })
    }
}
impl DataComponentImpl for ItemModelImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::String(self.id.clone().into_owned().into())
    }
    fn get_hash(&self) -> i32 {
        get_str_hash(self.id.as_ref()) as i32
    }
    default_impl!(ItemModel);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct LoreImpl {
    pub lines: Vec<TextComponent>,
}
impl LoreImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let NbtTag::List(lines) = data else {
            return None;
        };

        Some(Self {
            lines: lines
                .iter()
                .filter_map(NbtTag::extract_string)
                .map(|line| TextComponent::text(line.to_owned()))
                .collect(),
        })
    }
}
impl DataComponentImpl for LoreImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::List(
            self.lines
                .iter()
                .map(|line| NbtTag::String(line.clone().get_text().into_boxed_str()))
                .collect(),
        )
    }
    default_impl!(Lore);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Rarity {
    #[default]
    Common = 0,
    Uncommon = 1,
    Rare = 2,
    Epic = 3,
}

impl Rarity {
    #[must_use]
    pub fn from_id(id: i32) -> Option<Self> {
        match id {
            0 => Some(Self::Common),
            1 => Some(Self::Uncommon),
            2 => Some(Self::Rare),
            3 => Some(Self::Epic),
            _ => None,
        }
    }

    #[must_use]
    pub fn to_id(self) -> i32 {
        self as i32
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "common" => Some(Self::Common),
            "uncommon" => Some(Self::Uncommon),
            "rare" => Some(Self::Rare),
            "epic" => Some(Self::Epic),
            _ => None,
        }
    }

    #[must_use]
    pub fn to_name(self) -> &'static str {
        match self {
            Self::Common => "common",
            Self::Uncommon => "uncommon",
            Self::Rare => "rare",
            Self::Epic => "epic",
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct RarityImpl {
    pub rarity: Rarity,
}

impl RarityImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let name = data.extract_string()?;
        Some(Self {
            rarity: Rarity::from_name(name)?,
        })
    }
}

impl DataComponentImpl for RarityImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::String(self.rarity.to_name().into())
    }

    fn get_hash(&self) -> i32 {
        crate::data_component_impl::get_i32_hash(self.rarity.to_id()) as i32
    }

    default_impl!(Rarity);
}

#[derive(Clone, Debug, PartialEq)]
pub struct CustomModelDataImpl {
    pub floats: Vec<f32>,
    pub flags: Vec<bool>,
    pub strings: Vec<String>,
    pub colors: Vec<i32>,
}
impl CustomModelDataImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let compound = data.extract_compound()?;
        let floats = compound
            .get_list("floats")
            .map(|l| l.iter().filter_map(NbtTag::extract_float).collect())
            .unwrap_or_default();
        let flags = compound
            .get_list("flags")
            .map(|l| l.iter().filter_map(NbtTag::extract_bool).collect())
            .unwrap_or_default();
        let strings = compound
            .get_list("strings")
            .map(|l| {
                l.iter()
                    .filter_map(|t| t.extract_string().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        // Vanilla encodes the color list as ints, but tolerate a packed int array too.
        let colors = if let Some(arr) = compound.get_int_array("colors") {
            arr.to_vec()
        } else if let Some(l) = compound.get_list("colors") {
            l.iter().filter_map(NbtTag::extract_int).collect()
        } else {
            Vec::new()
        };
        Some(Self {
            floats,
            flags,
            strings,
            colors,
        })
    }
}
impl DataComponentImpl for CustomModelDataImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        compound.put_list(
            "floats",
            self.floats.iter().map(|f| NbtTag::Float(*f)).collect(),
        );
        compound.put_list(
            "flags",
            self.flags.iter().map(|b| NbtTag::Byte(*b as i8)).collect(),
        );
        compound.put_list(
            "strings",
            self.strings
                .iter()
                .map(|s| NbtTag::String(s.clone().into()))
                .collect(),
        );
        compound.put_list(
            "colors",
            self.colors.iter().map(|c| NbtTag::Int(*c)).collect(),
        );
        NbtTag::Compound(compound)
    }
    default_impl!(CustomModelData);
}

#[derive(Clone, Hash, PartialEq, Eq)]
pub struct TooltipDisplayImpl {
    pub hide_tooltip: bool,
    pub hidden_components: Vec<DataComponent>,
}
impl std::fmt::Debug for TooltipDisplayImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TooltipDisplayImpl")
            .field("hide_tooltip", &self.hide_tooltip)
            .field(
                "hidden_components",
                &self
                    .hidden_components
                    .iter()
                    .map(|id| id.to_name())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}
impl TooltipDisplayImpl {
    pub const DEFAULT: Self = Self {
        hide_tooltip: false,
        hidden_components: Vec::new(),
    };

    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let compound = data.extract_compound()?;
        let hide_tooltip = compound
            .get("hide_tooltip")
            .map_or(Some(false), NbtTag::extract_bool)?;
        let mut hidden_components = Vec::new();
        if let Some(tag) = compound.get("hidden_components") {
            for component in tag.extract_list()? {
                let name = component.extract_string()?;
                let id = DataComponent::try_from_name(name)?;
                if !hidden_components.contains(&id) {
                    hidden_components.push(id);
                }
            }
        }
        Some(Self {
            hide_tooltip,
            hidden_components,
        })
    }
}
impl DataComponentImpl for TooltipDisplayImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        compound.put_bool("hide_tooltip", self.hide_tooltip);
        compound.put_list(
            "hidden_components",
            self.hidden_components
                .iter()
                .map(|id| NbtTag::String(id.to_name().into()))
                .collect(),
        );
        NbtTag::Compound(compound)
    }
    default_impl!(TooltipDisplay);
}

#[allow(non_upper_case_globals)]
pub const TooltipDisplayImpl: TooltipDisplayImpl = TooltipDisplayImpl::DEFAULT;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct CreativeSlotLockImpl;
impl CreativeSlotLockImpl {
    pub const fn read_data(_data: &NbtTag) -> Option<Self> {
        Some(Self)
    }
}
impl DataComponentImpl for CreativeSlotLockImpl {
    default_impl!(CreativeSlotLock);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct EnchantmentGlintOverrideImpl;
impl EnchantmentGlintOverrideImpl {
    pub const fn read_data(_data: &NbtTag) -> Option<Self> {
        Some(Self)
    }
}
impl DataComponentImpl for EnchantmentGlintOverrideImpl {
    default_impl!(EnchantmentGlintOverride);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct TooltipStyleImpl {
    pub id: String,
}
impl TooltipStyleImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        data.extract_string().map(|id| Self { id: id.to_string() })
    }
}
impl DataComponentImpl for TooltipStyleImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::String(self.id.clone().into())
    }
    fn get_hash(&self) -> i32 {
        get_str_hash(&self.id) as i32
    }
    default_impl!(TooltipStyle);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct NoteBlockSoundImpl {
    pub sound: String,
}
impl NoteBlockSoundImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        data.extract_string().map(|sound| Self {
            sound: sound.to_string(),
        })
    }
}
impl DataComponentImpl for NoteBlockSoundImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::String(self.sound.clone().into())
    }
    fn get_hash(&self) -> i32 {
        get_str_hash(&self.sound) as i32
    }
    default_impl!(NoteBlockSound);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct BaseColorImpl {
    pub color: String,
}
impl BaseColorImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        data.extract_string().map(|color| Self {
            color: color.to_string(),
        })
    }
}
impl DataComponentImpl for BaseColorImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::String(self.color.clone().into())
    }
    fn get_hash(&self) -> i32 {
        get_str_hash(&self.color) as i32
    }
    default_impl!(BaseColor);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct InstrumentImpl;
impl InstrumentImpl {
    pub const fn read_data(_data: &NbtTag) -> Option<Self> {
        Some(Self)
    }
}
impl DataComponentImpl for InstrumentImpl {
    default_impl!(Instrument);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ProvidesTrimMaterialImpl;
impl ProvidesTrimMaterialImpl {
    pub const fn read_data(_data: &NbtTag) -> Option<Self> {
        Some(Self)
    }
}
impl DataComponentImpl for ProvidesTrimMaterialImpl {
    default_impl!(ProvidesTrimMaterial);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ProvidesBannerPatternsImpl;
impl ProvidesBannerPatternsImpl {
    pub const fn read_data(_data: &NbtTag) -> Option<Self> {
        Some(Self)
    }
}
impl DataComponentImpl for ProvidesBannerPatternsImpl {
    default_impl!(ProvidesBannerPatterns);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct BannerPatternLayer {
    pub pattern: String,
    pub color: crate::dye_color::DyeColor,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, Default)]
pub struct BannerPatternsImpl {
    pub layers: Vec<BannerPatternLayer>,
}

impl BannerPatternsImpl {
    pub const EMPTY: Self = Self { layers: Vec::new() };

    #[must_use]
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let mut layers = Vec::new();
        if let NbtTag::List(list) = data {
            for tag in list {
                if let Some(compound) = tag.extract_compound() {
                    let pattern = compound.get_string("pattern")?.to_string();
                    let color_str = compound.get_string("color")?;
                    let color = crate::dye_color::DyeColor::by_name(color_str).unwrap_or_default();
                    layers.push(BannerPatternLayer { pattern, color });
                }
            }
        }
        Some(Self { layers })
    }
}

impl DataComponentImpl for BannerPatternsImpl {
    fn write_data(&self) -> NbtTag {
        let mut list = Vec::new();
        for layer in &self.layers {
            let mut compound = NbtCompound::new();
            compound.put_string("pattern", layer.pattern.clone());
            compound.put_string("color", layer.color.name().to_string());
            list.push(NbtTag::Compound(compound));
        }
        NbtTag::List(list)
    }

    default_impl!(BannerPatterns);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct PotDecorationsImpl;
impl PotDecorationsImpl {
    pub const fn read_data(_data: &NbtTag) -> Option<Self> {
        Some(Self)
    }
}
impl DataComponentImpl for PotDecorationsImpl {
    default_impl!(PotDecorations);
}

/// The lock's item predicate, kept as its raw NBT compound since Pumpkin does
/// not yet model item predicates.
// TODO: replace `predicate` with a typed item predicate once item predicates are modelled.
#[derive(Clone, Debug, PartialEq)]
pub struct LockImpl {
    pub predicate: NbtCompound,
}
impl LockImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        data.extract_compound().map(|predicate| Self {
            predicate: predicate.clone(),
        })
    }
}
impl DataComponentImpl for LockImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::Compound(self.predicate.clone())
    }
    default_impl!(Lock);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct BreakSoundImpl;
impl BreakSoundImpl {
    pub const fn read_data(_data: &NbtTag) -> Option<Self> {
        Some(Self)
    }
}
impl DataComponentImpl for BreakSoundImpl {
    default_impl!(BreakSound);
}

#[derive(Clone, Debug, PartialEq)]
pub struct SoundEvent {
    pub sound_name: String,
    pub range: Option<f32>,
}
impl std::hash::Hash for SoundEvent {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.sound_name.hash(state);
        if let Some(val) = self.range {
            true.hash(state);
            unsafe { (*(&raw const val).cast::<u32>()).hash(state) };
        } else {
            false.hash(state);
        }
    }
}
impl SoundEvent {
    pub const fn new(sound_name: String, range: Option<f32>) -> Self {
        Self { sound_name, range }
    }
}
