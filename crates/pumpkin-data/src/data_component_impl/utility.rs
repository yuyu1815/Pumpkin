use crate::data_component_impl::{DataComponentImpl, get_i32_hash, get_str_hash};
use crc_fast::CrcAlgorithm::Crc32Iscsi;
use crc_fast::Digest;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_nbt::tag::NbtTag;
use pumpkin_util::identifier::Identifier;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct DyeImpl;
impl DyeImpl {
    pub const fn read_data(_data: &NbtTag) -> Option<Self> {
        Some(Self)
    }
}
impl DataComponentImpl for DyeImpl {
    default_impl!(Dye);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct DyedColorImpl {
    pub rgb: i32,
}
impl DyedColorImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        data.extract_int().map(|rgb| Self { rgb })
    }
}
impl DataComponentImpl for DyedColorImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::Int(self.rgb)
    }
    fn get_hash(&self) -> i32 {
        get_i32_hash(self.rgb) as i32
    }
    default_impl!(DyedColor);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct MapColorImpl;
impl MapColorImpl {
    pub const fn read_data(_data: &NbtTag) -> Option<Self> {
        Some(Self)
    }
}
impl DataComponentImpl for MapColorImpl {
    default_impl!(MapColor);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct MapIdImpl {
    pub id: i32,
}
impl MapIdImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        data.extract_int().map(|id| Self { id })
    }
}
impl DataComponentImpl for MapIdImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::Int(self.id)
    }
    fn get_hash(&self) -> i32 {
        get_i32_hash(self.id) as i32
    }
    default_impl!(MapId);
}

#[derive(Clone, Debug, PartialEq)]
pub struct MapDecorationEntry {
    pub decoration_type: Identifier,
    pub x: f64,
    pub z: f64,
    pub rotation: f32,
}

#[derive(Clone, Debug)]
pub struct MapDecorationsImpl {
    pub decorations: Vec<(String, MapDecorationEntry)>,
}

impl PartialEq for MapDecorationsImpl {
    fn eq(&self, other: &Self) -> bool {
        self.canonical_entries() == other.canonical_entries()
    }
}

impl MapDecorationsImpl {
    pub const EMPTY: Self = Self {
        decorations: Vec::new(),
    };

    fn canonical_entries(&self) -> Vec<(&str, &MapDecorationEntry)> {
        let mut entries = Vec::new();
        for (key, entry) in &self.decorations {
            if let Some(existing) = entries.iter_mut().find(|(existing, _)| *existing == key) {
                existing.1 = entry;
            } else {
                entries.push((key.as_str(), entry));
            }
        }
        entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        entries
    }

    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let root = data.extract_compound()?;
        let decorations = root
            .child_tags
            .iter()
            .map(|(key, value)| {
                let entry = value.extract_compound()?;
                Some((
                    key.to_string(),
                    MapDecorationEntry {
                        decoration_type: Identifier::parse(entry.get_string("type")?).ok()?,
                        x: nbt_number_as_f64(entry.get("x")?)?,
                        z: nbt_number_as_f64(entry.get("z")?)?,
                        rotation: nbt_number_as_f32(entry.get("rotation")?)?,
                    },
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self { decorations })
    }
}

fn nbt_number_as_f64(value: &NbtTag) -> Option<f64> {
    Some(match value {
        NbtTag::Byte(value) => *value as f64,
        NbtTag::Short(value) => *value as f64,
        NbtTag::Int(value) => *value as f64,
        NbtTag::Long(value) => *value as f64,
        NbtTag::Float(value) => *value as f64,
        NbtTag::Double(value) => *value,
        _ => return None,
    })
}

fn nbt_number_as_f32(value: &NbtTag) -> Option<f32> {
    Some(match value {
        NbtTag::Byte(value) => *value as f32,
        NbtTag::Short(value) => *value as f32,
        NbtTag::Int(value) => *value as f32,
        NbtTag::Long(value) => *value as f32,
        NbtTag::Float(value) => *value,
        NbtTag::Double(value) => *value as f32,
        _ => return None,
    })
}

impl DataComponentImpl for MapDecorationsImpl {
    fn write_data(&self) -> NbtTag {
        let mut root = NbtCompound::new();
        for (key, entry) in self.canonical_entries() {
            let mut value = NbtCompound::new();
            value.put_string("type", entry.decoration_type.to_string());
            value.put_double("x", entry.x);
            value.put_double("z", entry.z);
            value.put_float("rotation", entry.rotation);
            root.put_compound(key, value);
        }
        NbtTag::Compound(root)
    }

    fn get_hash(&self) -> i32 {
        let mut digest = Digest::new(Crc32Iscsi);
        digest.update(&[13u8]);
        for (key, entry) in self.canonical_entries() {
            digest.update(&get_str_hash(key).to_le_bytes());
            digest.update(&get_str_hash(&entry.decoration_type.to_string()).to_le_bytes());
            digest.update(&canonical_f64_bits(entry.x).to_le_bytes());
            digest.update(&canonical_f64_bits(entry.z).to_le_bytes());
            digest.update(&canonical_f32_bits(entry.rotation).to_le_bytes());
        }
        digest.finalize() as i32
    }

    default_impl!(MapDecorations);
}

fn canonical_f64_bits(value: f64) -> u64 {
    if value == 0.0 {
        0
    } else if value.is_nan() {
        f64::NAN.to_bits()
    } else {
        value.to_bits()
    }
}

fn canonical_f32_bits(value: f32) -> u32 {
    if value == 0.0 {
        0
    } else if value.is_nan() {
        f32::NAN.to_bits()
    } else {
        value.to_bits()
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum MapPostProcessing {
    Lock = 0,
    Scale = 1,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Default)]
pub struct MapPostProcessingImpl {
    pub processing: Option<MapPostProcessing>,
}

impl MapPostProcessingImpl {
    pub const SCALE: Self = Self {
        processing: Some(MapPostProcessing::Scale),
    };
    pub const LOCK: Self = Self {
        processing: Some(MapPostProcessing::Lock),
    };

    #[must_use]
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        if let Some(val) = data.extract_int() {
            let processing = match val {
                0 => Some(MapPostProcessing::Lock),
                1 => Some(MapPostProcessing::Scale),
                _ => None,
            };
            Some(Self { processing })
        } else if let Some(val) = data.extract_string() {
            let processing = match val {
                "lock" => Some(MapPostProcessing::Lock),
                "scale" => Some(MapPostProcessing::Scale),
                _ => None,
            };
            Some(Self { processing })
        } else {
            None
        }
    }
}

impl DataComponentImpl for MapPostProcessingImpl {
    fn write_data(&self) -> NbtTag {
        match self.processing {
            Some(MapPostProcessing::Lock) => NbtTag::Int(0),
            Some(MapPostProcessing::Scale) => NbtTag::Int(1),
            None => NbtTag::Int(0),
        }
    }
    default_impl!(MapPostProcessing);
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChargedProjectilesImpl {
    pub projectiles: Vec<NbtCompound>,
}
impl ChargedProjectilesImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let list = data.extract_list()?;
        let mut projectiles = Vec::new();
        for item in list {
            projectiles.push(item.extract_compound()?.clone());
        }
        Some(Self { projectiles })
    }
}
impl DataComponentImpl for ChargedProjectilesImpl {
    fn write_data(&self) -> NbtTag {
        let mut list = Vec::new();
        for item in &self.projectiles {
            list.push(NbtTag::Compound(item.clone()));
        }
        NbtTag::List(list)
    }
    fn get_hash(&self) -> i32 {
        0
    }
    default_impl!(ChargedProjectiles);
}

#[derive(Clone)]
pub struct BundleContentsImpl {
    pub items: Vec<crate::item_stack::ItemStack>,
}
impl PartialEq for BundleContentsImpl {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}
impl Eq for BundleContentsImpl {}
impl std::fmt::Debug for BundleContentsImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BundleContentsImpl")
    }
}
impl BundleContentsImpl {
    pub fn read_data(tag: &NbtTag) -> Option<Self> {
        let mut items = Vec::new();
        if let NbtTag::List(l) = tag {
            for item_tag in l {
                if let NbtTag::Compound(c) = item_tag
                    && let Some(stack) = crate::item_stack::ItemStack::read_item_stack(c)
                {
                    items.push(stack);
                }
            }
        }
        Some(Self { items })
    }
    pub fn get_weight(&self) -> u32 {
        self.items
            .iter()
            .map(|item| item.item_count as u32 * (64 / item.get_max_stack_size() as u32).max(1))
            .sum()
    }
    pub fn try_insert(&mut self, stack: &mut crate::item_stack::ItemStack) -> bool {
        if stack.is_empty() || stack.get_data_component::<BundleContentsImpl>().is_some() {
            return false;
        }
        let weight_per_item = (64 / stack.get_max_stack_size() as u32).max(1);
        let mut inserted_anything = false;
        while stack.item_count > 0 && self.get_weight() + weight_per_item <= 64 {
            if let Some(top) = self.items.first_mut()
                && crate::item_stack::ItemStack::are_items_and_components_equal(top, stack)
                && top.item_count < top.get_max_stack_size()
            {
                top.item_count += 1;
                stack.item_count -= 1;
                inserted_anything = true;
                continue;
            }
            self.items.insert(0, stack.copy_with_count(1));
            stack.item_count -= 1;
            inserted_anything = true;
        }
        inserted_anything
    }
    pub fn try_extract(&mut self) -> Option<crate::item_stack::ItemStack> {
        if self.items.is_empty() {
            None
        } else {
            Some(self.items.remove(0))
        }
    }
}
impl DataComponentImpl for BundleContentsImpl {
    fn write_data(&self) -> NbtTag {
        let mut list = Vec::new();
        for stack in &self.items {
            let mut item_compound = NbtCompound::new();
            stack.write_item_stack(&mut item_compound);
            list.push(NbtTag::Compound(item_compound));
        }
        NbtTag::List(list)
    }
    default_impl!(BundleContents);
}

/// The dimension and block position a lodestone compass points to.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct LodestoneTarget {
    pub dimension: String,
    pub x: i32,
    pub y: i32,
    pub z: i32,
}
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct LodestoneTrackerImpl {
    pub target: Option<LodestoneTarget>,
    pub tracked: bool,
}
impl LodestoneTrackerImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let compound = data.extract_compound()?;
        let tracked = compound.get_bool("tracked").unwrap_or(true);
        let target = compound.get_compound("target").and_then(|target| {
            let dimension = target.get_string("dimension")?.to_string();
            let pos = target.get_int_array("pos")?;
            if pos.len() != 3 {
                return None;
            }
            Some(LodestoneTarget {
                dimension,
                x: pos[0],
                y: pos[1],
                z: pos[2],
            })
        });
        Some(Self { target, tracked })
    }
}
impl DataComponentImpl for LodestoneTrackerImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        if let Some(target) = &self.target {
            let mut target_compound = NbtCompound::new();
            target_compound.put_string("dimension", target.dimension.clone());
            target_compound.put("pos", NbtTag::IntArray(vec![target.x, target.y, target.z]));
            compound.put_compound("target", target_compound);
        }
        compound.put_bool("tracked", self.tracked);
        NbtTag::Compound(compound)
    }
    default_impl!(LodestoneTracker);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FireworkExplosionShape {
    SmallBall = 0,
    LargeBall = 1,
    Star = 2,
    Creeper = 3,
    Burst = 4,
}
impl FireworkExplosionShape {
    pub fn from_id(id: i32) -> Option<Self> {
        match id {
            0 => Some(Self::SmallBall),
            1 => Some(Self::LargeBall),
            2 => Some(Self::Star),
            3 => Some(Self::Creeper),
            4 => Some(Self::Burst),
            _ => None,
        }
    }
    pub fn to_id(&self) -> i32 {
        *self as i32
    }
    pub fn to_name(&self) -> &str {
        match self {
            Self::SmallBall => "small_ball",
            Self::LargeBall => "large_ball",
            Self::Star => "star",
            Self::Creeper => "creeper",
            Self::Burst => "burst",
        }
    }
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "small_ball" => Some(Self::SmallBall),
            "large_ball" => Some(Self::LargeBall),
            "star" => Some(Self::Star),
            "creeper" => Some(Self::Creeper),
            "burst" => Some(Self::Burst),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct FireworkExplosionImpl {
    pub shape: FireworkExplosionShape,
    pub colors: Vec<i32>,
    pub fade_colors: Vec<i32>,
    pub has_trail: bool,
    pub has_twinkle: bool,
}
impl FireworkExplosionImpl {
    pub fn new(
        shape: FireworkExplosionShape,
        colors: Vec<i32>,
        fade_colors: Vec<i32>,
        has_trail: bool,
        has_twinkle: bool,
    ) -> Self {
        Self {
            shape,
            colors,
            fade_colors,
            has_trail,
            has_twinkle,
        }
    }
    pub fn read_data(tag: &NbtTag) -> Option<Self> {
        let compound = tag.extract_compound()?;
        let shape = FireworkExplosionShape::from_name(compound.get_string("shape")?)?;
        let colors = compound
            .get_int_array("colors")
            .map(|v| v.to_vec())
            .unwrap_or_default();
        let fade_colors = compound
            .get_int_array("fade_colors")
            .map(|v| v.to_vec())
            .unwrap_or_default();
        let has_trail = compound.get_bool("has_trail").unwrap_or(false);
        let has_twinkle = compound.get_bool("has_twinkle").unwrap_or(false);
        Some(Self {
            shape,
            colors,
            fade_colors,
            has_trail,
            has_twinkle,
        })
    }
}
impl DataComponentImpl for FireworkExplosionImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        compound.put_string("shape", self.shape.to_name().to_string());
        compound.put("colors", NbtTag::IntArray(self.colors.clone()));
        compound.put("fade_colors", NbtTag::IntArray(self.fade_colors.clone()));
        compound.put_bool("has_trail", self.has_trail);
        compound.put_bool("has_twinkle", self.has_twinkle);
        NbtTag::Compound(compound)
    }
    fn get_hash(&self) -> i32 {
        let mut digest = Digest::new(Crc32Iscsi);
        digest.update(&[2u8]);
        digest.update(&[self.shape.to_id() as u8]);
        for color in &self.colors {
            digest.update(&get_i32_hash(*color).to_le_bytes());
        }
        digest.update(&[3u8]);
        for color in &self.fade_colors {
            digest.update(&get_i32_hash(*color).to_le_bytes());
        }
        digest.update(&[4u8]);
        digest.update(&[self.has_trail as u8]);
        digest.update(&[self.has_twinkle as u8]);
        digest.finalize() as i32
    }
    default_impl!(FireworkExplosion);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct FireworksImpl {
    pub flight_duration: i32,
    pub explosions: Vec<FireworkExplosionImpl>,
}
impl FireworksImpl {
    pub fn new(flight_duration: i32, explosions: Vec<FireworkExplosionImpl>) -> Self {
        Self {
            flight_duration,
            explosions,
        }
    }
    pub fn read_data(tag: &NbtTag) -> Option<Self> {
        let compound = tag.extract_compound()?;
        let flight_duration = compound
            .get_byte("flight_duration")
            .map(i32::from)
            .or_else(|| compound.get_int("flight_duration"))
            .unwrap_or(1);
        let mut explosions = Vec::new();
        if let Some(list) = compound.get_list("explosions") {
            for item in list {
                if let Some(explosion) = FireworkExplosionImpl::read_data(item) {
                    explosions.push(explosion);
                }
            }
        }
        Some(Self {
            flight_duration,
            explosions,
        })
    }
}
impl DataComponentImpl for FireworksImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        compound.put_int("flight_duration", self.flight_duration);
        let explosions_list: Vec<NbtTag> = self.explosions.iter().map(|e| e.write_data()).collect();
        compound.put_list("explosions", explosions_list);
        NbtTag::Compound(compound)
    }
    fn get_hash(&self) -> i32 {
        let mut digest = Digest::new(Crc32Iscsi);
        digest.update(&[2u8]);
        digest.update(&get_i32_hash(self.flight_duration).to_le_bytes());
        for explosion in &self.explosions {
            digest.update(&get_i32_hash(explosion.get_hash()).to_le_bytes());
        }
        digest.update(&[3u8]);
        digest.finalize() as i32
    }
    default_impl!(Fireworks);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ProfileProperty {
    pub name: String,
    pub value: String,
    pub signature: Option<String>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, Default)]
pub struct ProfileImpl {
    pub name: Option<String>,
    pub id: Option<[i32; 4]>,
    pub properties: Vec<ProfileProperty>,
    pub texture: Option<String>,
    pub cape: Option<String>,
    pub elytra: Option<String>,
    pub model: Option<String>,
}
impl ProfileImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        match data {
            NbtTag::String(name) => Some(Self {
                name: Some(name.to_string()),
                ..Default::default()
            }),
            NbtTag::Compound(compound) => {
                let name = compound.get_string("name").map(String::from);
                let id = compound.get_int_array("id").and_then(|arr| {
                    if arr.len() == 4 {
                        Some([arr[0], arr[1], arr[2], arr[3]])
                    } else {
                        None
                    }
                });
                let mut properties = Vec::new();
                if let Some(props_list) = compound.get_list("properties") {
                    for prop_tag in props_list {
                        if let Some(prop_comp) = prop_tag.extract_compound()
                            && let (Some(prop_name), Some(prop_value)) =
                                (prop_comp.get_string("name"), prop_comp.get_string("value"))
                        {
                            properties.push(ProfileProperty {
                                name: prop_name.to_string(),
                                value: prop_value.to_string(),
                                signature: prop_comp.get_string("signature").map(String::from),
                            });
                        }
                    }
                }
                let texture = compound.get_string("texture").map(String::from);
                let cape = compound.get_string("cape").map(String::from);
                let elytra = compound.get_string("elytra").map(String::from);
                let model = compound.get_string("model").map(String::from);
                Some(Self {
                    name,
                    id,
                    properties,
                    texture,
                    cape,
                    elytra,
                    model,
                })
            }
            _ => None,
        }
    }
}
impl DataComponentImpl for ProfileImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        if let Some(name) = &self.name {
            compound.put_string("name", name.clone());
        }
        if let Some(id) = &self.id {
            compound.put("id", NbtTag::IntArray(id.to_vec()));
        }
        if !self.properties.is_empty() {
            let mut props_list = Vec::new();
            for prop in &self.properties {
                let mut prop_comp = NbtCompound::new();
                prop_comp.put_string("name", prop.name.clone());
                prop_comp.put_string("value", prop.value.clone());
                if let Some(sig) = &prop.signature {
                    prop_comp.put_string("signature", sig.clone());
                }
                props_list.push(NbtTag::Compound(prop_comp));
            }
            compound.put_list("properties", props_list);
        }
        if let Some(texture) = &self.texture {
            compound.put_string("texture", texture.clone());
        }
        if let Some(cape) = &self.cape {
            compound.put_string("cape", cape.clone());
        }
        if let Some(elytra) = &self.elytra {
            compound.put_string("elytra", elytra.clone());
        }
        if let Some(model) = &self.model {
            compound.put_string("model", model.clone());
        }
        NbtTag::Compound(compound)
    }
    fn get_hash(&self) -> i32 {
        let mut digest = Digest::new(Crc32Iscsi);
        if let Some(name) = &self.name {
            digest.update(&[1u8]);
            digest.update(&get_str_hash(name).to_le_bytes());
        }
        if let Some(id) = &self.id {
            digest.update(&[2u8]);
            for val in id {
                digest.update(&get_i32_hash(*val).to_le_bytes());
            }
        }
        if !self.properties.is_empty() {
            digest.update(&[3u8]);
            for prop in &self.properties {
                digest.update(&get_str_hash(&prop.name).to_le_bytes());
                digest.update(&get_str_hash(&prop.value).to_le_bytes());
                if let Some(sig) = &prop.signature {
                    digest.update(&[1u8]);
                    digest.update(&get_str_hash(sig).to_le_bytes());
                } else {
                    digest.update(&[0u8]);
                }
            }
        }
        if let Some(texture) = &self.texture {
            digest.update(&[4u8]);
            digest.update(&get_str_hash(texture).to_le_bytes());
        }
        if let Some(cape) = &self.cape {
            digest.update(&[5u8]);
            digest.update(&get_str_hash(cape).to_le_bytes());
        }
        if let Some(elytra) = &self.elytra {
            digest.update(&[6u8]);
            digest.update(&get_str_hash(elytra).to_le_bytes());
        }
        if let Some(model) = &self.model {
            digest.update(&[7u8]);
            digest.update(&get_str_hash(model).to_le_bytes());
        }
        digest.finalize() as i32
    }
    default_impl!(Profile);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct JukeboxPlayableImpl {
    pub song: &'static str,
}
impl JukeboxPlayableImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let compound = data.extract_compound()?;
        let song = compound.get_string("song")?;
        let static_song = crate::jukebox_song::JukeboxSong::from_name(
            song.strip_prefix("minecraft:").unwrap_or(song),
        )?
        .to_identifier();
        Some(Self { song: static_song })
    }
}
impl DataComponentImpl for JukeboxPlayableImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        compound.put_string(
            "song",
            format!(
                "minecraft:{}",
                self.song.strip_prefix("minecraft:").unwrap_or(self.song)
            ),
        );
        NbtTag::Compound(compound)
    }

    default_impl!(JukeboxPlayable);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RecipesImpl {
    pub recipes: Vec<String>,
}
impl RecipesImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let NbtTag::List(values) = data else {
            return None;
        };
        let recipes = values
            .iter()
            .map(|value| {
                let value = value.extract_string()?;
                Some(Identifier::parse(value).ok()?.to_string())
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self { recipes })
    }
}
impl DataComponentImpl for RecipesImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::List(
            self.recipes
                .iter()
                .map(|recipe| NbtTag::String(recipe.clone().into_boxed_str()))
                .collect(),
        )
    }
    default_impl!(Recipes);
}
