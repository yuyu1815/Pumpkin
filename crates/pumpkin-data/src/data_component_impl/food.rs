use crate::data_component_impl::basic::SoundEvent;
use crate::data_component_impl::{
    DataComponentImpl, IDSet, IdOr, get_f32_hash, get_i32_hash, get_idor, get_idor_hash,
    get_idset_hash, get_str_hash, put_idor,
};
use crate::effect::StatusEffect;
use crate::sound::Sound;
use crc_fast::CrcAlgorithm::Crc32Iscsi;
use crc_fast::Digest;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_nbt::tag::NbtTag;
use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

#[derive(Clone, Debug, PartialEq)]
pub struct FoodImpl {
    pub nutrition: i32,
    pub saturation: f32,
    pub can_always_eat: bool,
}
impl FoodImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let compound = data.extract_compound()?;
        let nutrition = compound.get_int("nutrition")?;
        let saturation = compound.get_float("saturation")?;
        let can_always_eat = compound.get_bool("can_always_eat").unwrap_or(false);
        Some(Self {
            nutrition,
            saturation,
            can_always_eat,
        })
    }
}
impl DataComponentImpl for FoodImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        compound.put_int("nutrition", self.nutrition);
        compound.put_float("saturation", self.saturation);
        compound.put_bool("can_always_eat", self.can_always_eat);
        NbtTag::Compound(compound)
    }
    default_impl!(Food);
}
impl Hash for FoodImpl {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.nutrition.hash(state);
        unsafe { (*(&raw const self.saturation).cast::<u32>()).hash(state) };
        self.can_always_eat.hash(state);
    }
}

fn nbt_number(tag: &NbtTag) -> Option<f64> {
    match tag {
        NbtTag::Byte(value) => Some(f64::from(*value)),
        NbtTag::Short(value) => Some(f64::from(*value)),
        NbtTag::Int(value) => Some(f64::from(*value)),
        NbtTag::Long(value) => Some(*value as f64),
        NbtTag::Float(value) => Some(f64::from(*value)),
        NbtTag::Double(value) => Some(*value),
        _ => None,
    }
}

fn nbt_i32(tag: &NbtTag) -> Option<i32> {
    match tag {
        NbtTag::Byte(value) => Some(i32::from(*value)),
        NbtTag::Short(value) => Some(i32::from(*value)),
        NbtTag::Int(value) => Some(*value),
        NbtTag::Long(value) => Some(*value as i32),
        NbtTag::Float(value) => Some(*value as i32),
        NbtTag::Double(value) => Some(*value as i32),
        _ => None,
    }
}

fn optional_i32(compound: &NbtCompound, key: &str, default: i32) -> Option<i32> {
    compound.get(key).map_or(Some(default), nbt_i32)
}

fn optional_f32(compound: &NbtCompound, key: &str, default: f32) -> Option<f32> {
    compound.get(key).map_or(Some(default), |tag| {
        nbt_number(tag).map(|value| value as f32)
    })
}

fn optional_bool(compound: &NbtCompound, key: &str, default: bool) -> Option<bool> {
    compound.get(key).map_or(Some(default), |tag| {
        nbt_number(tag).map(|value| value != 0.0)
    })
}

pub struct StatusEffectInstance {
    pub effect_id: Cow<'static, str>,
    pub amplifier: i32,
    pub duration: i32,
    pub ambient: bool,
    pub show_particles: bool,
    pub show_icon: bool,
    pub hidden_effect: Option<Box<StatusEffectInstance>>,
}

impl Clone for StatusEffectInstance {
    fn clone(&self) -> Self {
        let mut nodes = Vec::new();
        let mut current = Some(self);
        while let Some(effect) = current {
            nodes.push(Self {
                effect_id: effect.effect_id.clone(),
                amplifier: effect.amplifier,
                duration: effect.duration,
                ambient: effect.ambient,
                show_particles: effect.show_particles,
                show_icon: effect.show_icon,
                hidden_effect: None,
            });
            current = effect.hidden_effect.as_deref();
        }

        let mut hidden_effect = None;
        for mut node in nodes.into_iter().rev() {
            node.hidden_effect = hidden_effect;
            hidden_effect = Some(Box::new(node));
        }
        hidden_effect
            .map(|effect| *effect)
            .expect("status effect always has a root")
    }
}

impl PartialEq for StatusEffectInstance {
    fn eq(&self, other: &Self) -> bool {
        let mut left = Some(self);
        let mut right = Some(other);
        loop {
            match (left, right) {
                (Some(left_node), Some(right_node)) => {
                    if left_node.effect_id != right_node.effect_id
                        || left_node.amplifier != right_node.amplifier
                        || left_node.duration != right_node.duration
                        || left_node.ambient != right_node.ambient
                        || left_node.show_particles != right_node.show_particles
                        || left_node.show_icon != right_node.show_icon
                    {
                        return false;
                    }
                    left = left_node.hidden_effect.as_deref();
                    right = right_node.hidden_effect.as_deref();
                }
                (None, None) => return true,
                _ => return false,
            }
        }
    }
}

impl Eq for StatusEffectInstance {}

impl Hash for StatusEffectInstance {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let mut current = Some(self);
        while let Some(effect) = current {
            effect.effect_id.hash(state);
            effect.amplifier.hash(state);
            effect.duration.hash(state);
            effect.ambient.hash(state);
            effect.show_particles.hash(state);
            effect.show_icon.hash(state);
            current = effect.hidden_effect.as_deref();
            current.is_some().hash(state);
        }
    }
}

impl Drop for StatusEffectInstance {
    fn drop(&mut self) {
        let mut hidden_effect = self.hidden_effect.take();
        while let Some(mut effect) = hidden_effect {
            hidden_effect = effect.hidden_effect.take();
        }
    }
}

impl std::fmt::Debug for StatusEffectInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut depth = 0;
        let mut current = Some(self);
        while let Some(effect) = current {
            depth += 1;
            current = effect.hidden_effect.as_deref();
        }
        formatter
            .debug_struct("StatusEffectInstance")
            .field("effect_id", &self.effect_id)
            .field("amplifier", &self.amplifier)
            .field("duration", &self.duration)
            .field("ambient", &self.ambient)
            .field("show_particles", &self.show_particles)
            .field("show_icon", &self.show_icon)
            .field("hidden_effect_depth", &depth)
            .finish()
    }
}

impl StatusEffectInstance {
    pub fn read_data(nbt: &NbtTag) -> Option<Self> {
        let root = nbt.extract_compound()?;
        let effect_id = StatusEffect::from_minecraft_name(root.get_string("id")?)?.minecraft_name;

        // NBT parsing already enforces MAX_NBT_DEPTH. Keep this walk iterative so the
        // recursive hidden_effect payload cannot grow the Rust call stack.
        let mut nodes = Vec::new();
        let mut current = Some(nbt);
        while let Some(tag) = current {
            let compound = tag.extract_compound()?;
            let amplifier = optional_i32(compound, "amplifier", 0)?;
            if !(0..=255).contains(&amplifier) {
                return None;
            }
            let duration = optional_i32(compound, "duration", 0)?;
            let ambient = optional_bool(compound, "ambient", false)?;
            let show_particles = optional_bool(compound, "show_particles", true)?;
            let show_icon = optional_bool(compound, "show_icon", show_particles)?;
            current = compound.get("hidden_effect");
            nodes.push(Self {
                effect_id: Cow::Borrowed(effect_id),
                amplifier,
                duration,
                ambient,
                show_particles,
                show_icon,
                hidden_effect: None,
            });
        }

        let mut hidden_effect = None;
        for mut node in nodes.into_iter().rev() {
            node.hidden_effect = hidden_effect;
            hidden_effect = Some(Box::new(node));
        }
        hidden_effect.map(|effect| *effect)
    }

    pub fn as_nbt(&self) -> NbtTag {
        let mut compounds = Vec::new();
        let mut current = Some(self);
        while let Some(effect) = current {
            let mut compound = NbtCompound::new();
            if compounds.is_empty() {
                compound.put_string("id", effect.effect_id.to_string());
            }
            compound.put_int("amplifier", effect.amplifier);
            compound.put_int("duration", effect.duration);
            compound.put_bool("ambient", effect.ambient);
            compound.put_bool("show_particles", effect.show_particles);
            compound.put_bool("show_icon", effect.show_icon);
            compounds.push(compound);
            current = effect.hidden_effect.as_deref();
        }

        let mut encoded = None;
        for mut compound in compounds.into_iter().rev() {
            if let Some(hidden) = encoded {
                compound.put("hidden_effect", NbtTag::Compound(hidden));
            }
            encoded = Some(compound);
        }
        NbtTag::Compound(encoded.expect("status effect always has a root"))
    }

    pub fn get_hash(&self) -> i32 {
        let mut digest = Digest::new(Crc32Iscsi);
        let mut current = Some(self);
        while let Some(effect) = current {
            digest.update(&get_str_hash(effect.effect_id.as_ref()).to_le_bytes());
            digest.update(&get_i32_hash(effect.amplifier).to_le_bytes());
            digest.update(&get_i32_hash(effect.duration).to_le_bytes());
            digest.update(&[effect.ambient as u8]);
            digest.update(&[effect.show_particles as u8]);
            digest.update(&[effect.show_icon as u8]);
            current = effect.hidden_effect.as_deref();
            if current.is_some() {
                digest.update(&[1]);
            }
        }
        digest.finalize() as i32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConsumeAnimation {
    None,
    Eat,
    Drink,
    Block,
    Bow,
    Spear,
    Crossbow,
    Spyglass,
    Horn,
    Brush,
}

impl ConsumeAnimation {
    #[must_use]
    pub const fn to_str(&self) -> &'static str {
        match self {
            ConsumeAnimation::None => "none",
            ConsumeAnimation::Eat => "eat",
            ConsumeAnimation::Drink => "drink",
            ConsumeAnimation::Block => "block",
            ConsumeAnimation::Bow => "bow",
            ConsumeAnimation::Spear => "spear",
            ConsumeAnimation::Crossbow => "crossbow",
            ConsumeAnimation::Spyglass => "spyglass",
            ConsumeAnimation::Horn => "horn",
            ConsumeAnimation::Brush => "brush",
        }
    }
}
impl TryFrom<i32> for ConsumeAnimation {
    type Error = ();
    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Eat),
            2 => Ok(Self::Drink),
            3 => Ok(Self::Block),
            4 => Ok(Self::Bow),
            5 => Ok(Self::Spear),
            6 => Ok(Self::Crossbow),
            7 => Ok(Self::Spyglass),
            8 => Ok(Self::Horn),
            9 => Ok(Self::Brush),
            _ => Err(()),
        }
    }
}
impl FromStr for ConsumeAnimation {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "eat" => Ok(Self::Eat),
            "drink" => Ok(Self::Drink),
            "block" => Ok(Self::Block),
            "bow" => Ok(Self::Bow),
            "spear" => Ok(Self::Spear),
            "crossbow" => Ok(Self::Crossbow),
            "spyglass" => Ok(Self::Spyglass),
            "horn" => Ok(Self::Horn),
            "brush" => Ok(Self::Brush),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConsumeEffect {
    ApplyEffects((Cow<'static, [StatusEffectInstance]>, f32)),
    RemoveEffects(IDSet<StatusEffect>),
    ClearAllEffects,
    TeleportRandomly(f32),
    PlaySound(IdOr<SoundEvent>),
}
impl Hash for ConsumeEffect {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.to_str().hash(state);
        match self {
            ConsumeEffect::ApplyEffects(tuple) => {
                tuple.0.hash(state);
                unsafe { (*(&raw const tuple.1).cast::<u32>()).hash(state) };
            }
            ConsumeEffect::RemoveEffects(status_effect_instances) => {
                status_effect_instances.hash(state)
            }
            ConsumeEffect::ClearAllEffects => (),
            ConsumeEffect::TeleportRandomly(dst) => unsafe {
                (*(&raw const dst).cast::<u32>()).hash(state)
            },
            ConsumeEffect::PlaySound(id_or) => id_or.hash(state),
        }
    }
}
impl ConsumeEffect {
    pub fn to_str(&self) -> &str {
        match self {
            ConsumeEffect::ApplyEffects(_) => "apply_effects",
            ConsumeEffect::RemoveEffects(_) => "remove_effects",
            ConsumeEffect::ClearAllEffects => "clear_all_effects",
            ConsumeEffect::TeleportRandomly(_) => "teleport_randomly",
            ConsumeEffect::PlaySound(_) => "play_sound",
        }
    }
    pub fn registry_id(&self) -> u8 {
        match self {
            ConsumeEffect::ApplyEffects(_) => 0,
            ConsumeEffect::RemoveEffects(_) => 1,
            ConsumeEffect::ClearAllEffects => 2,
            ConsumeEffect::TeleportRandomly(_) => 3,
            ConsumeEffect::PlaySound(_) => 4,
        }
    }
    pub fn read_data(nbt: &NbtTag) -> Option<Self> {
        let compound = nbt.extract_compound()?;
        let r#type = compound.get_string("type")?;
        let r#type = r#type.strip_prefix("minecraft:").unwrap_or(r#type);
        match r#type {
            "remove_effects" => {
                let idset = IDSet::read(compound.get("effects")?)?;
                Some(Self::RemoveEffects(idset))
            }
            "clear_all_effects" => Some(Self::ClearAllEffects),
            "teleport_randomly" => {
                let dst = optional_f32(compound, "diameter", 16.0)?;
                if !(dst > 0.0 && dst <= f32::MAX) {
                    return None;
                }
                Some(Self::TeleportRandomly(dst))
            }
            "play_sound" => {
                let sound = crate::data_component_impl::get_idor_strict(compound, "sound")?;
                Some(Self::PlaySound(sound))
            }
            "apply_effects" => {
                let probability = optional_f32(compound, "probability", 1.0)?;
                if !(0.0..=1.0).contains(&probability) {
                    return None;
                }
                let effects_vec: Vec<StatusEffectInstance> = compound
                    .get_list("effects")?
                    .iter()
                    .map(StatusEffectInstance::read_data)
                    .collect::<Option<Vec<_>>>()?;
                let effects: Cow<'static, [StatusEffectInstance]> = Cow::Owned(effects_vec);
                Some(Self::ApplyEffects((effects, probability)))
            }
            _ => None,
        }
    }
    pub fn as_nbt(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        compound.put_string("type", format!("minecraft:{}", self.to_str()));
        match self {
            ConsumeEffect::ApplyEffects(data) => {
                let nbt_arr = data.0.iter().map(|x| x.as_nbt()).collect();
                compound.put_list("effects", nbt_arr);
                compound.put_float("probability", data.1);
            }
            ConsumeEffect::RemoveEffects(idset) => idset.write(&mut compound, "effects"),
            ConsumeEffect::ClearAllEffects => (),
            ConsumeEffect::TeleportRandomly(dst) => compound.put_float("diameter", *dst),
            ConsumeEffect::PlaySound(id_or) => {
                put_idor(&mut compound, "sound", id_or);
            }
        }
        NbtTag::Compound(compound)
    }
    pub fn get_hash(&self) -> i32 {
        let mut digest = Digest::new(Crc32Iscsi);
        match self {
            ConsumeEffect::ApplyEffects((effects, probability)) => {
                digest.update(&[1u8]);
                for effect in effects.iter() {
                    digest.update(&effect.get_hash().to_le_bytes());
                }
                digest.update(&[13u8]);
                digest.update(&get_f32_hash(*probability).to_le_bytes());
            }
            ConsumeEffect::RemoveEffects(idset) => {
                digest.update(&[2u8]);
                digest.update(&get_idset_hash(idset).to_le_bytes());
            }
            ConsumeEffect::ClearAllEffects => {
                digest.update(&[3u8]);
            }
            ConsumeEffect::TeleportRandomly(dst) => {
                digest.update(&[4u8]);
                digest.update(&get_f32_hash(*dst).to_le_bytes());
            }
            ConsumeEffect::PlaySound(id_or) => {
                digest.update(&[5u8]);
                digest.update(&get_idor_hash(id_or).to_le_bytes());
            }
        }
        digest.finalize() as i32
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConsumableImpl {
    pub consume_seconds: f32,
    pub animation: ConsumeAnimation,
    pub sound_event: IdOr<SoundEvent>,
    pub consume_particles: bool,
    pub effects: Cow<'static, [ConsumeEffect]>,
}
impl ConsumableImpl {
    pub const fn new(
        consume_seconds: f32,
        animation: ConsumeAnimation,
        sound_event: IdOr<SoundEvent>,
        consume_particles: bool,
        effects: Cow<'static, [ConsumeEffect]>,
    ) -> Self {
        Self {
            consume_seconds,
            animation,
            sound_event,
            consume_particles,
            effects,
        }
    }
    #[must_use]
    pub fn consume_ticks(&self) -> i32 {
        (self.consume_seconds * 20.0) as i32
    }
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let compound = data.extract_compound()?;
        let consume_seconds = compound.get_float("consume_seconds")?;
        let animation = compound
            .get_string("animation")?
            .parse::<ConsumeAnimation>()
            .ok()?;
        let sound_event = get_idor(compound, "sound", Sound::EntityGenericEat);
        let consume_particles = compound.get_bool("has_consume_particles").unwrap_or(false);
        let opt_list = compound.get_list("on_consume_effects");
        let effects: Cow<'static, [ConsumeEffect]> = if let Some(effect_list) = opt_list {
            Cow::Owned(
                effect_list
                    .iter()
                    .map(ConsumeEffect::read_data)
                    .collect::<Option<Vec<_>>>()?,
            )
        } else {
            Cow::Borrowed(&[])
        };
        Some(Self {
            consume_seconds,
            animation,
            sound_event,
            consume_particles,
            effects,
        })
    }
}
impl DataComponentImpl for ConsumableImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        compound.put_float("consume_seconds", self.consume_seconds);
        compound.put_string("animation", self.animation.to_str().to_string());
        put_idor(&mut compound, "sound", &self.sound_event);
        compound.put_bool("has_consume_particles", self.consume_particles);
        let nbt_vec = self.effects.iter().map(|x| x.as_nbt()).collect();
        compound.put_list("on_consume_effects", nbt_vec);
        NbtTag::Compound(compound)
    }
    fn get_hash(&self) -> i32 {
        let mut digest = Digest::new(Crc32Iscsi);
        digest.update(&[2u8]);
        digest.update(&get_f32_hash(self.consume_seconds).to_le_bytes());
        digest.update(&get_i32_hash(self.animation as i32).to_le_bytes());
        digest.update(&get_idor_hash(&self.sound_event).to_be_bytes());
        digest.update(&[self.consume_particles as u8]);
        for effect in self.effects.iter() {
            digest.update(&effect.get_hash().to_le_bytes());
        }
        digest.update(&[3u8]);
        digest.finalize() as i32
    }
    default_impl!(Consumable);
}
impl Hash for ConsumableImpl {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        unsafe { (*(&raw const self.consume_seconds).cast::<u32>()).hash(state) };
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct UseEffectsImpl;
impl UseEffectsImpl {
    pub const fn read_data(_data: &NbtTag) -> Option<Self> {
        Some(Self)
    }
}
impl DataComponentImpl for UseEffectsImpl {
    default_impl!(UseEffects);
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct UseRemainderImpl;
impl UseRemainderImpl {
    pub const fn read_data(_data: &NbtTag) -> Option<Self> {
        Some(Self)
    }
}
impl DataComponentImpl for UseRemainderImpl {
    default_impl!(UseRemainder);
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UseCooldownImpl {
    pub seconds: f32,
    pub cooldown_group: Option<String>,
}
impl UseCooldownImpl {
    pub fn new(seconds: f32, cooldown_group: Option<String>) -> Self {
        Self {
            seconds,
            cooldown_group,
        }
    }
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        let compound = data.extract_compound()?;
        let seconds = compound.get_float("seconds")?;
        let cooldown_group = compound.get_string("cooldown_group").map(|s| s.to_string());
        Some(Self {
            seconds,
            cooldown_group,
        })
    }
}
impl DataComponentImpl for UseCooldownImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        compound.put_float("seconds", self.seconds);
        if let Some(group) = &self.cooldown_group {
            compound.put_string("cooldown_group", group.clone());
        }
        NbtTag::Compound(compound)
    }
    fn get_hash(&self) -> i32 {
        let mut digest = Digest::new(Crc32Iscsi);
        digest.update(&get_f32_hash(self.seconds).to_le_bytes());
        if let Some(group) = &self.cooldown_group {
            digest.update(&get_str_hash(group).to_le_bytes());
        }
        digest.finalize() as i32
    }
    default_impl!(UseCooldown);
}
impl Eq for UseCooldownImpl {}
impl Hash for UseCooldownImpl {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.seconds.to_bits().hash(state);
        self.cooldown_group.hash(state);
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct PotionContentsImpl {
    pub potion_id: Option<i32>,
    pub custom_color: Option<i32>,
    pub custom_effects: Vec<StatusEffectInstance>,
    pub custom_name: Option<String>,
}
impl PotionContentsImpl {
    pub fn read_data(tag: &NbtTag) -> Option<Self> {
        let compound = tag.extract_compound()?;
        let potion_id = if let Some(id) = compound.get_int("potion") {
            Some(id)
        } else if let Some(name) = compound.get_string("potion") {
            let name = name.strip_prefix("minecraft:").unwrap_or(name);
            crate::potion::Potion::from_name(name).map(|p| p.id as i32)
        } else {
            None
        };
        let custom_color = compound.get_int("custom_color");
        let custom_name = compound.get_string("custom_name").map(|s| s.to_string());
        let custom_effects = match compound.get_list("custom_effects") {
            Some(list) => list
                .iter()
                .map(StatusEffectInstance::read_data)
                .collect::<Option<Vec<_>>>()?,
            None => Vec::new(),
        };
        Some(Self {
            potion_id,
            custom_color,
            custom_effects,
            custom_name,
        })
    }
}
impl DataComponentImpl for PotionContentsImpl {
    fn write_data(&self) -> NbtTag {
        let mut compound = NbtCompound::new();
        if let Some(potion_id) = self.potion_id {
            compound.put_int("potion", potion_id);
        }
        if let Some(color) = self.custom_color {
            compound.put_int("custom_color", color);
        }
        if !self.custom_effects.is_empty() {
            compound.put_list(
                "custom_effects",
                self.custom_effects
                    .iter()
                    .map(StatusEffectInstance::as_nbt)
                    .collect(),
            );
        }
        if let Some(name) = &self.custom_name {
            compound.put_string("custom_name", name.clone());
        }
        NbtTag::Compound(compound)
    }
    fn get_hash(&self) -> i32 {
        let mut digest = Digest::new(Crc32Iscsi);
        if let Some(id) = self.potion_id {
            digest.update(&[1u8]);
            digest.update(&get_i32_hash(id).to_le_bytes());
        }
        if let Some(color) = self.custom_color {
            digest.update(&[2u8]);
            digest.update(&get_i32_hash(color).to_le_bytes());
        }
        if let Some(name) = &self.custom_name {
            digest.update(&[3u8]);
            digest.update(&get_str_hash(name).to_le_bytes());
        }
        if !self.custom_effects.is_empty() {
            digest.update(&[4u8]);
            for effect in &self.custom_effects {
                digest.update(&effect.get_hash().to_le_bytes());
            }
        }
        digest.finalize() as i32
    }
    default_impl!(PotionContents);
}

#[derive(Clone, Debug, PartialEq)]
pub struct PotionDurationScaleImpl {
    pub scale: f32,
}
impl PotionDurationScaleImpl {
    pub fn read_data(data: &NbtTag) -> Option<Self> {
        data.extract_float().map(|scale| Self { scale })
    }
}
impl DataComponentImpl for PotionDurationScaleImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::Float(self.scale)
    }
    fn get_hash(&self) -> i32 {
        get_f32_hash(self.scale) as i32
    }
    default_impl!(PotionDurationScale);
}
impl Hash for PotionDurationScaleImpl {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.scale.to_bits().hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DataComponentImpl, PotionContentsImpl, PotionDurationScaleImpl, StatusEffectInstance,
    };
    use crate::item::Item;
    use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};
    use std::borrow::Cow;
    use std::hash::Hasher;

    #[test]
    fn potion_duration_scale_round_trips_as_a_float() {
        let scale = PotionDurationScaleImpl { scale: 0.125 };
        let encoded = scale.write_data();
        let decoded = PotionDurationScaleImpl::read_data(&encoded).expect("scale should decode");

        assert_eq!(decoded, scale);
    }

    #[test]
    fn generated_arrow_duration_scale_is_data_driven() {
        let scale = Item::TIPPED_ARROW
            .components
            .iter()
            .find_map(|(id, component)| {
                (*id == crate::data_component::DataComponent::PotionDurationScale)
                    .then(|| component.as_any().downcast_ref::<PotionDurationScaleImpl>())
                    .flatten()
            })
            .expect("tipped arrows should have a duration scale");

        assert_eq!(scale.scale, 0.125);
    }

    fn hidden_effect_fixture(id: &str) -> NbtTag {
        let mut hidden_2 = NbtCompound::new();
        hidden_2.put_int("amplifier", 0);
        hidden_2.put_int("duration", 20);
        hidden_2.put_bool("ambient", false);
        hidden_2.put_bool("show_particles", true);
        hidden_2.put_bool("show_icon", true);

        let mut hidden_1 = NbtCompound::new();
        hidden_1.put_int("amplifier", 0);
        hidden_1.put_int("duration", 40);
        hidden_1.put_bool("ambient", false);
        hidden_1.put_bool("show_particles", true);
        hidden_1.put_bool("show_icon", true);
        hidden_1.put("hidden_effect", NbtTag::Compound(hidden_2));

        let mut root = NbtCompound::new();
        root.put_string("id", id.to_owned());
        root.put_int("amplifier", 1);
        root.put_int("duration", 100);
        root.put_bool("ambient", false);
        root.put_bool("show_particles", true);
        root.put_bool("show_icon", true);
        root.put("hidden_effect", NbtTag::Compound(hidden_1));

        let mut component = NbtCompound::new();
        component.put_list("custom_effects", vec![NbtTag::Compound(root)]);
        NbtTag::Compound(component)
    }

    #[test]
    fn potion_contents_preserves_two_hidden_effect_links_in_nbt() {
        let input = hidden_effect_fixture("minecraft:regeneration");
        let decoded = PotionContentsImpl::read_data(&input).expect("fixture should decode");
        assert_eq!(decoded.custom_effects.len(), 1);
        assert_eq!(
            decoded.custom_effects[0]
                .hidden_effect
                .as_ref()
                .and_then(|effect| effect.hidden_effect.as_ref())
                .map(|effect| effect.duration),
            Some(20)
        );
        assert_eq!(decoded.write_data(), input);
    }

    #[test]
    fn potion_contents_rejects_invalid_hidden_effect_and_unknown_id() {
        let mut root = NbtCompound::new();
        root.put_string("id", "minecraft:regeneration".to_owned());
        root.put_string("hidden_effect", "not a compound".to_owned());
        let mut wrong_type_component = NbtCompound::new();
        wrong_type_component.put_list("custom_effects", vec![NbtTag::Compound(root)]);
        assert!(PotionContentsImpl::read_data(&NbtTag::Compound(wrong_type_component)).is_none());
        assert!(
            PotionContentsImpl::read_data(&hidden_effect_fixture("minecraft:not_real")).is_none()
        );
    }

    #[test]
    fn potion_contents_applies_effect_defaults_and_preserves_name() {
        let mut effect = NbtCompound::new();
        effect.put_string("id", "minecraft:regeneration".to_owned());
        let mut component = NbtCompound::new();
        component.put_list("custom_effects", vec![NbtTag::Compound(effect)]);
        component.put_string("custom_name", "test potion".to_owned());

        let decoded = PotionContentsImpl::read_data(&NbtTag::Compound(component))
            .expect("official optional fields should default");
        let effect = &decoded.custom_effects[0];
        assert_eq!(effect.amplifier, 0);
        assert_eq!(effect.duration, 0);
        assert!(!effect.ambient);
        assert!(effect.show_particles);
        assert!(effect.show_icon);
        assert_eq!(decoded.custom_name.as_deref(), Some("test potion"));
    }

    #[test]
    fn status_effect_chain_operations_are_iterative() {
        let mut chain = None;
        for _ in (0..4096).rev() {
            chain = Some(Box::new(StatusEffectInstance {
                effect_id: Cow::Borrowed("minecraft:regeneration"),
                amplifier: 0,
                duration: 1,
                ambient: false,
                show_particles: true,
                show_icon: true,
                hidden_effect: chain,
            }));
        }
        let effect = *chain.expect("chain should have a root");
        let cloned = effect.clone();
        assert_eq!(effect, cloned);

        let mut left = std::collections::hash_map::DefaultHasher::new();
        let mut right = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&effect, &mut left);
        std::hash::Hash::hash(&cloned, &mut right);
        assert_eq!(left.finish(), right.finish());
        assert!(format!("{effect:?}").contains("hidden_effect_depth"));
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct SuspiciousStewEffect {
    pub effect: Cow<'static, str>,
    pub duration: i32,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct SuspiciousStewEffectsImpl {
    pub effects: Cow<'static, [SuspiciousStewEffect]>,
}
impl SuspiciousStewEffectsImpl {
    pub const EMPTY: Self = Self {
        effects: Cow::Borrowed(&[]),
    };

    pub fn read_data(data: &NbtTag) -> Option<Self> {
        Some(Self {
            effects: Cow::Owned(
                data.extract_list()?
                    .iter()
                    .filter_map(|tag| {
                        let effect = tag.extract_compound()?;
                        Some(SuspiciousStewEffect {
                            effect: Cow::Owned(effect.get_string("id")?.to_owned()),
                            duration: effect.get_int("duration").unwrap_or(160),
                        })
                    })
                    .collect(),
            ),
        })
    }
}
impl DataComponentImpl for SuspiciousStewEffectsImpl {
    fn write_data(&self) -> NbtTag {
        NbtTag::List(
            self.effects
                .iter()
                .map(|effect| {
                    let mut nbt = NbtCompound::new();
                    nbt.put_string("id", effect.effect.to_string());
                    nbt.put_int("duration", effect.duration);
                    NbtTag::Compound(nbt)
                })
                .collect(),
        )
    }
    fn get_hash(&self) -> i32 {
        let mut digest = Digest::new(Crc32Iscsi);
        for effect in self.effects.iter() {
            digest.update(&get_str_hash(&effect.effect).to_le_bytes());
            digest.update(&get_i32_hash(effect.duration).to_le_bytes());
        }
        digest.finalize() as i32
    }
    default_impl!(SuspiciousStewEffects);
}
