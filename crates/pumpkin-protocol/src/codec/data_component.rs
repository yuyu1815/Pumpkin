#![allow(clippy::wildcard_imports)]

use std::borrow::Cow;
use std::cell::Cell;

use crate::codec::var_int::VarInt;
use crate::ser::{NetworkReadExt, NetworkWriteExt, ReadingError, WritingError};
use pumpkin_data::data_component::DataComponent;
use pumpkin_data::data_component_impl::*;
use pumpkin_data::jukebox_song::JukeboxSong;
use pumpkin_data::{Block, BlockId, Enchantment};

use pumpkin_data::effect::StatusEffect;
use pumpkin_data::entity::EntityType;
use pumpkin_data::sound::Sound;
use pumpkin_nbt::{compound::NbtCompound, serializer::NbtWriteHelperJava, tag::NbtTag};
use pumpkin_util::identifier::Identifier;
use pumpkin_util::version::JavaMinecraftVersion;

const MAX_STATUS_EFFECTS: usize = 128;
// Implementation ceiling retained from the former iterative skip codec; not an official limit.
const MAX_EFFECT_DEPTH: usize = 32;
const MAX_IDSET_ELEMENTS: usize = 256;
const MAX_DEATH_EFFECTS: usize = 256;
const MAX_TOOLTIP_HIDDEN_COMPONENTS: usize = 256;
// Implementation ceilings for recursive ItemStack templates; these are not official maxima.
const MAX_ITEM_STACK_TEMPLATE_DEPTH: usize = 64;

thread_local! {
    static ITEM_STACK_TEMPLATE_DEPTH: Cell<usize> = const { Cell::new(0) };
}

struct ItemStackTemplateDepth;
impl ItemStackTemplateDepth {
    fn enter() -> Result<Self, ()> {
        ITEM_STACK_TEMPLATE_DEPTH.with(|depth| {
            let current = depth.get();
            if current >= MAX_ITEM_STACK_TEMPLATE_DEPTH {
                return Err(());
            }
            depth.set(current + 1);
            Ok(Self)
        })
    }
}
impl Drop for ItemStackTemplateDepth {
    fn drop(&mut self) {
        ITEM_STACK_TEMPLATE_DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

#[must_use]
pub fn data_to_proto_sound(id_or: &IdOr<SoundEvent>) -> crate::IdOr<crate::SoundEvent> {
    match id_or {
        IdOr::Id(id) => crate::IdOr::Id(*id as u16),
        IdOr::Value(sound) => crate::IdOr::Value(crate::SoundEvent {
            sound_name: sound.sound_name.clone(),
            range: sound.range,
        }),
    }
}

#[must_use]
pub fn proto_to_data_sound(id_or: &crate::IdOr<crate::SoundEvent>) -> Option<IdOr<SoundEvent>> {
    match id_or {
        crate::IdOr::Id(id) => {
            let name = Sound::NAMES.get(*id as usize)?;
            Some(IdOr::Id(Sound::from_name(name)?))
        }
        crate::IdOr::Value(sound) => Some(IdOr::Value(SoundEvent {
            sound_name: sound.sound_name.clone(),
            range: sound.range,
        })),
    }
}

fn deserialize_idset<T: IDSetContent>(
    seq: &mut impl NetworkReadExt,
) -> Result<IDSet<T>, ReadingError> {
    let id_type = seq.get_var_int()?.0;

    match id_type.cmp(&0) {
        std::cmp::Ordering::Equal => {
            let tag = seq.get_str()?;
            Ok(IDSet::Tag(Cow::Owned(tag.into())))
        }
        std::cmp::Ordering::Greater => {
            let len = id_type - 1;
            if len as usize > MAX_IDSET_ELEMENTS {
                return Err(ReadingError::Message("Too many IDSet elements".into()));
            }
            let mut content_vec = Vec::with_capacity(len as usize);

            for _ in 0..len {
                let varint_id = seq.get_var_int()?.0;

                let elmt = T::from_id(varint_id as u16).ok_or(ReadingError::Message(
                    "Invalid registry id VarInt in IDSet".into(),
                ))?;
                content_vec.push(elmt);
            }
            Ok(IDSet::IDs(Cow::Owned(content_vec)))
        }
        std::cmp::Ordering::Less => Result::Err(ReadingError::Message(
            "Negative type/len VarInt in IDSet".into(),
        )),
    }
}

fn serialize_idset<C: IDSetContent>(
    idset: &IDSet<C>,
    seq: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    match idset {
        IDSet::Tag(tag) => {
            seq.write_var_int(&VarInt(0))?;
            seq.write_string(tag)
        }
        IDSet::IDs(elements) => {
            if elements.len() > MAX_IDSET_ELEMENTS {
                return Err(WritingError::Message("Too many IDSet elements".into()));
            }
            let count = i32::try_from(elements.len() + 1)
                .map_err(|_| WritingError::Message("Too many IDSet elements".into()))?;
            seq.write_var_int(&VarInt(count))?;
            for elmt in elements.iter() {
                seq.write_var_int(&VarInt(elmt.registry_id() as i32))?;
            }
            Ok(())
        }
    }
}

fn deserialize_status_effect(
    seq: &mut impl NetworkReadExt,
    effect_name: &'static str,
) -> Result<StatusEffectInstance, ReadingError> {
    let mut nodes = Vec::new();
    let mut depth = 0;

    loop {
        let amplifier = seq.get_var_int()?.0;
        let duration = seq.get_var_int()?.0;
        let ambient = seq.get_bool()?;
        let show_particles = seq.get_bool()?;
        let show_icon = seq.get_bool()?;
        let has_hidden = seq.get_bool()?;
        nodes.push(StatusEffectInstance {
            effect_id: Cow::Borrowed(effect_name),
            amplifier,
            duration,
            ambient,
            show_particles,
            show_icon,
            hidden_effect: None,
        });
        if !has_hidden {
            break;
        }
        depth += 1;
        if depth > MAX_EFFECT_DEPTH {
            return Err(ReadingError::TooLarge(
                "Potion effect hidden depth exceeded".into(),
            ));
        }
    }

    let mut hidden_effect = None;
    for mut node in nodes.into_iter().rev() {
        node.hidden_effect = hidden_effect;
        hidden_effect = Some(Box::new(node));
    }
    hidden_effect
        .map(|effect| *effect)
        .ok_or_else(|| ReadingError::Message("Missing status effect details".into()))
}

fn deserialize_status_effects(
    seq: &mut impl NetworkReadExt,
) -> Result<Vec<StatusEffectInstance>, ReadingError> {
    let effects_len = seq.get_var_int()?.0 as usize;
    if effects_len > MAX_STATUS_EFFECTS {
        return Err(ReadingError::Message("Too many status effects".into()));
    }
    let mut custom_effects = Vec::with_capacity(effects_len);
    for _ in 0..effects_len {
        let effect_registry_id = u16::try_from(seq.get_var_int()?.0)
            .map_err(|_| ReadingError::Message("Invalid effect_id!".into()))?;
        let effect_name = StatusEffect::from_id(effect_registry_id)
            .ok_or(ReadingError::Message("Invalid effect_id!".into()))?
            .minecraft_name;
        custom_effects.push(deserialize_status_effect(seq, effect_name)?);
    }

    Ok(custom_effects)
}

fn serialize_status_effects(
    effects: &[StatusEffectInstance],
    seq: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    seq.write_var_int(&VarInt(effects.len() as i32))?;

    for effect in effects {
        let effect_name =
            StatusEffect::from_minecraft_name(&effect.effect_id).ok_or_else(|| {
                WritingError::Message(format!("Invalid status effect: {}", effect.effect_id))
            })?;
        seq.write_var_int(&VarInt(effect_name.registry_id() as i32))?;

        let mut current = Some(effect);
        while let Some(effect) = current {
            if effect.effect_id.as_ref() != effect_name.minecraft_name {
                return Err(WritingError::Message(
                    "Hidden status effect has a different effect id".into(),
                ));
            }
            seq.write_var_int(&VarInt::from(effect.amplifier))?;
            seq.write_var_int(&VarInt::from(effect.duration))?;
            seq.write_bool(effect.ambient)?;
            seq.write_bool(effect.show_particles)?;
            seq.write_bool(effect.show_icon)?;
            current = effect.hidden_effect.as_deref();
            seq.write_bool(current.is_some())?;
        }
    }
    Ok(())
}

fn deserialize_consume_effect(
    seq: &mut impl NetworkReadExt,
) -> Result<ConsumeEffect, ReadingError> {
    let effect_type = seq.get_var_int()?.0;
    match effect_type {
        0 => {
            let effects = deserialize_status_effects(seq)?;
            let probability = seq.get_f32()?;
            Ok(ConsumeEffect::ApplyEffects((
                Cow::Owned(effects),
                probability,
            )))
        }
        1 => {
            let idset = deserialize_idset(seq)?;
            Ok(ConsumeEffect::RemoveEffects(idset))
        }
        2 => Ok(ConsumeEffect::ClearAllEffects),
        3 => {
            let diameter = seq.get_f32()?;
            Ok(ConsumeEffect::TeleportRandomly(diameter))
        }
        4 => {
            // Need to read IdOr<SoundEvent> manually. This depends on how it is serialized.
            // In vanilla, it's either an id (0) or a sound event (1) ... but wait, `crate::IdOr<crate::SoundEvent>` doesn't have a `NetworkReadExt` method.
            // Let's defer this and assume it implements `read` for now or wait, `IdOr` does implement `PacketRead` or something?
            // Actually, we can just use `IdOr::read` if we impl it, but let's change it to:
            let proto_sound_event = crate::IdOr::<crate::SoundEvent>::read(seq, |r| {
                let sound_name = r.get_str()?.into();
                let range = r.get_option(NetworkReadExt::get_f32)?;
                Ok(crate::SoundEvent { sound_name, range })
            })
            .map_err(|e| {
                ReadingError::Message(format!("No sound IdOr<SoundEvent> in ConsumeEffect: {e}"))
            })?;
            Ok(ConsumeEffect::PlaySound(
                proto_to_data_sound(&proto_sound_event).ok_or(ReadingError::Message(
                    "Invalid sound in ConsumeEffect".into(),
                ))?,
            ))
        }
        _ => Err(ReadingError::Message(
            "Invalid effect_type in ConsumeEffect".into(),
        )),
    }
}

fn serialize_consume_effect(
    consume_effect: &ConsumeEffect,
    seq: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    seq.write_var_int(&VarInt(consume_effect.registry_id() as i32))?;
    match consume_effect {
        ConsumeEffect::ApplyEffects((effects, probability)) => {
            serialize_status_effects(effects, seq)?;
            seq.write_f32(*probability)?;
        }
        ConsumeEffect::RemoveEffects(idset) => serialize_idset(idset, seq)?,
        ConsumeEffect::ClearAllEffects => (),
        ConsumeEffect::TeleportRandomly(diameter) => seq.write_f32(*diameter)?,
        ConsumeEffect::PlaySound(id_or) => {
            crate::IdOr::<crate::SoundEvent>::write(&data_to_proto_sound(id_or), seq, |w, e| {
                w.write_string(&e.sound_name)?;
                w.write_option(&e.range, |w2, r| w2.write_f32(*r))
            })?;
        }
    }
    Ok(())
}

pub(crate) trait DataComponentCodec<Impl: DataComponentImpl> {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError>;
    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Impl, ReadingError>;
}

fn serialize_nbt_fallback<T: DataComponentImpl>(
    value: &T,
    seq: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    seq.write_nbt(value.write_data())
}

fn deserialize_nbt_fallback<T>(
    seq: &mut impl NetworkReadExt,
    component_name: &str,
    read_data: fn(&NbtTag) -> Option<T>,
) -> Result<T, ReadingError> {
    let tag = seq
        .get_nbt_with_version(&JavaMinecraftVersion::V_26_2)?
        .ok_or_else(|| ReadingError::Message(format!("Missing {component_name} component NBT")))?;
    read_data(&tag)
        .ok_or_else(|| ReadingError::Message(format!("Invalid {component_name} component NBT")))
}

impl DataComponentCodec<Self> for MaxStackSizeImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.size))
    }
    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let size = u8::try_from(seq.get_var_int()?.0)
            .map_err(|_| ReadingError::Message("No MaxStackSize VarInt!".into()))?;
        Ok(Self { size })
    }
}

impl DataComponentCodec<Self> for DamageImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.damage))
    }
    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let damage = seq.get_var_int()?.0;
        Ok(Self { damage })
    }
}

impl DataComponentCodec<Self> for RepairCostImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.cost))
    }
    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let cost = seq.get_var_int()?.0;
        Ok(Self { cost })
    }
}

impl DataComponentCodec<Self> for EnchantmentsImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.enchantment.len() as i32))?;
        for (enc, level) in self.enchantment.iter() {
            seq.write_var_int(&VarInt::from(enc.id))?;
            seq.write_var_int(&VarInt::from(*level))?;
        }
        Ok(())
    }
    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        const MAX_ENCHANTMENTS: usize = 256;

        let len = seq.get_var_int()?.0 as usize;
        if len > MAX_ENCHANTMENTS {
            return Err(ReadingError::Message("Too many enchantments".into()));
        }
        let mut enc = Vec::with_capacity(len);
        for _ in 0..len {
            let id = seq.get_var_int()?.0 as u8;
            let level = seq.get_var_int()?.0;
            enc.push((
                Enchantment::from_id(id).ok_or(ReadingError::Message(
                    "EnchantmentsImpl Enchantment VarInt Incorrect!".into(),
                ))?,
                level,
            ));
        }
        Ok(Self {
            enchantment: Cow::from(enc),
        })
    }
}

impl DataComponentCodec<Self> for UnbreakableImpl {
    fn serialize(&self, _seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        Ok(())
    }
    fn deserialize(_seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for ItemModelImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_string(&self.id)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let id = seq.get_str()?;
        Ok(Self {
            id: Cow::Owned(id.into()),
        })
    }
}

impl DataComponentCodec<Self> for CustomNameImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        let mut bytes = Vec::new();
        NbtTag::String(self.name.clone().get_text().into_boxed_str())
            .serialize(&mut NbtWriteHelperJava::new(&mut bytes))
            .map_err(|e| WritingError::Message(e.to_string()))?;
        seq.write_slice(&bytes)?;
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let tag = seq.get_nbt_with_version(&pumpkin_util::version::JavaMinecraftVersion::V_26_2)?;
        let name = tag.as_ref().map_or_else(
            pumpkin_util::text::TextComponent::empty,
            pumpkin_util::text::TextComponent::from_nbt,
        );
        Ok(Self { name })
    }
}

impl DataComponentCodec<Self> for LoreImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt(self.lines.len() as i32))?;
        for line in &self.lines {
            seq.write_slice(
                &line.encode_for_version(&pumpkin_util::version::JavaMinecraftVersion::V_26_2),
            )?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        // TODO: Could probably be extracted?
        const MAX_LORE_LINES: i32 = 256;

        let count = seq.get_var_int()?.0;
        if !(0..=MAX_LORE_LINES).contains(&count) {
            return Err(ReadingError::Message(format!(
                "LoreImpl line count {count} is out of bounds (0-{MAX_LORE_LINES})"
            )));
        }

        let mut lines = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let tag =
                seq.get_nbt_with_version(&pumpkin_util::version::JavaMinecraftVersion::V_26_2)?;
            let text = tag.as_ref().map_or_else(
                pumpkin_util::text::TextComponent::empty,
                pumpkin_util::text::TextComponent::from_nbt,
            );
            lines.push(text);
        }
        Ok(Self { lines })
    }
}

impl DataComponentCodec<Self> for ItemNameImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        let tag = self
            .name
            .as_component()
            .to_nbt_tag_for_version(&JavaMinecraftVersion::V_26_2);
        let mut bytes = Vec::new();
        tag.serialize(&mut NbtWriteHelperJava::new(&mut bytes))
            .map_err(|error| WritingError::Message(error.to_string()))?;
        seq.write_slice(&bytes)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let tag = seq
            .get_nbt_with_version(&JavaMinecraftVersion::V_26_2)?
            .ok_or_else(|| ReadingError::Message("Missing ItemName NBT".into()))?;
        if !matches!(tag, NbtTag::String(_) | NbtTag::Compound(_) | NbtTag::List(_)) {
            return Err(ReadingError::Message("Invalid ItemName NBT tag type".into()));
        }
        let name = pumpkin_util::text::TextComponent::try_from_nbt(&tag)
            .map_err(|error| ReadingError::Message(format!("Invalid ItemName component: {error}")))?;
        Ok(Self {
            name: pumpkin_data::data_component_impl::ItemName::Component(name),
        })
    }
}

impl DataComponentCodec<Self> for DyedColorImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_i32(self.rgb)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        Ok(Self {
            rgb: seq.get_i32()?,
        })
    }
}

impl DataComponentCodec<Self> for SuspiciousStewEffectsImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        let effect_count = i32::try_from(self.effects.len())
            .map_err(|_| WritingError::Message("Too many suspicious stew effects".into()))?;
        seq.write_var_int(&VarInt(effect_count))?;
        for effect in self.effects.iter() {
            let id = StatusEffect::from_minecraft_name(&effect.effect)
                .ok_or_else(|| WritingError::Message("Unknown suspicious stew effect".into()))?
                .id;
            seq.write_var_int(&VarInt(i32::from(id)))?;
            seq.write_var_int(&VarInt(effect.duration))?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        const MAX_EFFECTS: i32 = 128;

        let count = seq.get_var_int()?.0;
        if !(0..=MAX_EFFECTS).contains(&count) {
            return Err(ReadingError::Message(
                "Invalid suspicious stew effect count".into(),
            ));
        }

        let mut effects =
            Vec::with_capacity(usize::try_from(count).map_err(|_| {
                ReadingError::Message("Invalid suspicious stew effect count".into())
            })?);
        for _ in 0..count {
            let id = u16::try_from(seq.get_var_int()?.0)
                .map_err(|_| ReadingError::Message("Invalid suspicious stew effect id".into()))?;
            let effect = StatusEffect::from_id(id)
                .ok_or_else(|| ReadingError::Message("Unknown suspicious stew effect id".into()))?;
            let duration = seq.get_var_int()?.0;
            effects.push(SuspiciousStewEffect {
                effect: Cow::Borrowed(effect.minecraft_name),
                duration,
            });
        }
        Ok(Self {
            effects: Cow::Owned(effects),
        })
    }
}

impl DataComponentCodec<Self> for CustomDataImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        let mut bytes = Vec::new();
        NbtTag::Compound(self.data.clone())
            .serialize(&mut NbtWriteHelperJava::new(&mut bytes))
            .map_err(|e| WritingError::Message(e.to_string()))?;
        seq.write_slice(&bytes)?;
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let data = seq
            .get_compound_nbt_with_version(&pumpkin_util::version::JavaMinecraftVersion::V_26_2)?
            .unwrap_or_else(pumpkin_nbt::compound::NbtCompound::new);
        Ok(Self { data })
    }
}

impl DataComponentCodec<Self> for ConsumableImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_f32(self.consume_seconds)?;
        seq.write_var_int(&VarInt(self.animation as i32))?;
        crate::IdOr::<crate::SoundEvent>::write(
            &data_to_proto_sound(&self.sound_event),
            seq,
            |w, e| {
                w.write_string(&e.sound_name)?;
                w.write_option(&e.range, |w2, r| w2.write_f32(*r))
            },
        )?;
        seq.write_bool(self.consume_particles)?;
        seq.write_var_int(&VarInt(self.effects.len() as i32))?;

        for effect in self.effects.iter() {
            serialize_consume_effect(effect, seq)?;
        }

        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        const MAX_CONSUME_EFFECTS: i32 = 256;

        let consume_seconds = seq.get_f32()?;
        let animation_id = seq.get_var_int()?;

        let animation: ConsumeAnimation = animation_id
            .0
            .try_into()
            .map_err(|()| ReadingError::Message("Invalid ConsumableImpl animation id!".into()))?;
        let proto_sound_event = crate::IdOr::<crate::SoundEvent>::read(seq, |r| {
            let sound_name = r.get_str()?.into();
            let range = r.get_option(NetworkReadExt::get_f32)?;
            Ok(crate::SoundEvent { sound_name, range })
        })?;
        let consume_particles = seq.get_bool()?;

        let sound_event = proto_to_data_sound(&proto_sound_event).ok_or(ReadingError::Message(
            "Invalid sound in ConsumableImpl".into(),
        ))?;
        let effects_len = seq.get_var_int()?.0;
        if !(0..=MAX_CONSUME_EFFECTS).contains(&effects_len) {
            return Err(ReadingError::Message("Invalid consume effect count".into()));
        }

        let mut effects_vec = Vec::with_capacity(effects_len as usize);

        for _ in 0..effects_len {
            effects_vec.push(deserialize_consume_effect(seq)?);
        }

        let effects: Cow<'static, [ConsumeEffect]> = Cow::Owned(effects_vec);

        Ok(Self {
            consume_seconds,
            animation,
            sound_event,
            consume_particles,
            effects,
        })
    }
}

impl DataComponentCodec<Self> for EquippableImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt(self.slot.get_slot_index()))?;
        crate::IdOr::<crate::SoundEvent>::write(
            &data_to_proto_sound(&self.equip_sound),
            seq,
            |w, e| {
                w.write_string(&e.sound_name)?;
                w.write_option(&e.range, |w2, r| w2.write_f32(*r))
            },
        )?;

        seq.write_bool(self.asset_id.is_some())?;
        if let Some(asset) = &self.asset_id {
            seq.write_string(asset)?;
        }

        seq.write_bool(self.camera_overlay.is_some())?;
        if let Some(overlay) = &self.camera_overlay {
            seq.write_string(overlay)?;
        }

        seq.write_bool(self.allowed_entities.is_some())?;
        if let Some(allowed) = &self.allowed_entities {
            serialize_idset(allowed, seq)?;
        }

        seq.write_bool(self.dispensable)?;
        seq.write_bool(self.swappable)?;
        seq.write_bool(self.damage_on_hurt)?;
        seq.write_bool(self.equip_on_interact)?;
        seq.write_bool(self.can_be_sheared)?;
        crate::IdOr::<crate::SoundEvent>::write(
            &data_to_proto_sound(&self.shearing_sound),
            seq,
            |w, e| {
                w.write_string(&e.sound_name)?;
                w.write_option(&e.range, |w2, r| w2.write_f32(*r))
            },
        )
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let slot_index = seq.get_var_int()?.0;
        let slot = EquipmentSlot::from_slot_index(slot_index).ok_or(ReadingError::Message(
            format!("Invalid equipment slot index {slot_index}"),
        ))?;
        let equip_sound = proto_to_data_sound(&crate::IdOr::<crate::SoundEvent>::read(seq, |r| {
            let sound_name = r.get_str()?.into();
            let range = r.get_option(NetworkReadExt::get_f32)?;
            Ok(crate::SoundEvent { sound_name, range })
        })?)
        .ok_or(ReadingError::Message(
            "Invalid sound in EquippableImpl".into(),
        ))?;

        let asset_id = if seq.get_bool()? {
            Some(Cow::Owned(seq.get_str()?.into()))
        } else {
            None
        };

        let camera_overlay = if seq.get_bool()? {
            Some(Cow::Owned(seq.get_str()?.into()))
        } else {
            None
        };

        let has_allowed_entities = seq.get_bool()?;

        let allowed_entities: Option<IDSet<EntityType>> = if has_allowed_entities {
            Some(deserialize_idset(seq)?)
        } else {
            None
        };

        let dispensable = seq.get_bool()?;
        let swappable = seq.get_bool()?;
        let damage_on_hurt = seq.get_bool()?;
        let equip_on_interact = seq.get_bool()?;
        let can_be_sheared = seq.get_bool()?;
        let shearing_sound =
            proto_to_data_sound(&crate::IdOr::<crate::SoundEvent>::read(seq, |r| {
                let sound_name = r.get_str()?.into();
                let range = r.get_option(NetworkReadExt::get_f32)?;
                Ok(crate::SoundEvent { sound_name, range })
            })?)
            .ok_or(ReadingError::Message(
                "Invalid shearing sound in EquippableImpl".into(),
            ))?;

        Ok(Self {
            slot,
            equip_sound,
            asset_id,
            camera_overlay,
            allowed_entities,
            dispensable,
            swappable,
            damage_on_hurt,
            equip_on_interact,
            can_be_sheared,
            shearing_sound,
        })
    }
}

impl DataComponentCodec<Self> for PotionContentsImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        // Potion ID (optional)
        if let Some(potion_id) = self.potion_id {
            seq.write_bool(true)?;
            seq.write_var_int(&VarInt::from(potion_id))?;
        } else {
            seq.write_bool(false)?;
        }

        // Custom color (optional)
        if let Some(color) = self.custom_color {
            seq.write_bool(true)?;
            seq.write_i32(color)?;
        } else {
            seq.write_bool(false)?;
        }

        // Custom effects list
        serialize_status_effects(&self.custom_effects, seq)?;

        // Custom name (optional)
        if let Some(name) = &self.custom_name {
            seq.write_bool(true)?;
            seq.write_string(name.as_str())?;
        } else {
            seq.write_bool(false)?;
        }

        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        // Potion ID (optional)
        let has_potion = seq.get_bool()?;
        let potion_id = has_potion
            .then(|| seq.get_var_int().map(|value| value.0))
            .transpose()?;

        // Custom color (optional)
        let has_color = seq.get_bool()?;
        let custom_color = has_color.then(|| seq.get_i32()).transpose()?;

        // Custom effects list
        let custom_effects = deserialize_status_effects(seq)?;

        // Custom name (optional)
        let has_name = seq.get_bool()?;
        let custom_name = has_name
            .then(|| seq.get_str().map(String::from))
            .transpose()?;

        Ok(Self {
            potion_id,
            custom_color,
            custom_effects,
            custom_name,
        })
    }
}

#[cfg(test)]
mod hidden_effect_tests {
    use super::{
        ConsumeEffect, DataComponentCodec, MAX_EFFECT_DEPTH, PotionContentsImpl, ReadingError,
        deserialize_consume_effect, deserialize_status_effect, serialize_consume_effect,
    };
    use std::io::Cursor;

    #[test]
    fn potion_contents_preserves_two_hidden_effect_links_on_wire() {
        let expected = [
            0x00, 0x00, 0x01, 0x09, 0x01, 0x64, 0x00, 0x01, 0x01, 0x01, 0x00, 0x28, 0x00, 0x01,
            0x01, 0x01, 0x00, 0x14, 0x00, 0x01, 0x01, 0x00, 0x00,
        ];
        let mut input = expected.as_slice();
        let decoded = PotionContentsImpl::deserialize(&mut input).expect("fixture should decode");
        assert!(input.is_empty());

        let mut encoded = Vec::new();
        decoded
            .serialize(&mut encoded)
            .expect("fixture should encode");
        assert_eq!(encoded, expected);
    }

    #[test]
    fn apply_effects_wire_order_is_effects_then_probability() {
        let expected = [
            0x00, // apply_effects
            0x01, 0x09, // one regeneration effect
            0x01, 0x64, 0x00, 0x01, 0x01, 0x00, // details
            0x3f, 0x80, 0x00, 0x00, // probability = 1.0f
        ];
        let mut input = expected.as_slice();
        let decoded = deserialize_consume_effect(&mut input).expect("fixture should decode");
        assert!(input.is_empty());
        match &decoded {
            ConsumeEffect::ApplyEffects((effects, probability)) => {
                assert_eq!(effects.len(), 1);
                assert_eq!(effects[0].duration, 100);
                assert_eq!(*probability, 1.0);
            }
            other => panic!("expected apply_effects, got {other:?}"),
        }

        let mut encoded = Vec::new();
        serialize_consume_effect(&decoded, &mut encoded).expect("fixture should encode");
        assert_eq!(encoded, expected);
    }

    fn status_effect_details(hidden_links: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity((hidden_links + 1) * 6);
        for link in 0..=hidden_links {
            bytes.extend([0, 0, 0, 1, 1, u8::from(link < hidden_links)]);
        }
        bytes
    }

    #[test]
    fn wire_hidden_effect_depth_limit_has_a_checked_boundary() {
        let mut at_limit = Cursor::new(status_effect_details(MAX_EFFECT_DEPTH));
        assert!(deserialize_status_effect(&mut at_limit, "minecraft:regeneration").is_ok());
        assert_eq!(at_limit.position() as usize, at_limit.get_ref().len());

        let mut over_limit = Cursor::new(status_effect_details(MAX_EFFECT_DEPTH + 1));
        assert!(matches!(
            deserialize_status_effect(&mut over_limit, "minecraft:regeneration"),
            Err(ReadingError::TooLarge(_))
        ));
    }
}

#[cfg(test)]
mod item_stack_template_tests {
    use super::{
        ChargedProjectilesImpl, ContainerImpl, CustomDataImpl, DataComponent, DataComponentCodec,
        ReadingError, SulfurCubeContentImpl, UnbreakableImpl, UseRemainderImpl,
        deserialize_item_stack_template, serialize_item_stack_template,
    };
    use pumpkin_data::data_component_impl::DataComponentImpl;
    use pumpkin_data::item::Item;
    use pumpkin_data::item_stack::ItemStack;
    use pumpkin_nbt::compound::NbtCompound;

    fn custom_stack() -> ItemStack {
        let mut data = NbtCompound::new();
        data.put_int("x", 7);
        ItemStack::new_with_component(
            2,
            &Item::BOWL,
            vec![(
                DataComponent::CustomData,
                Some(CustomDataImpl { data }.to_dyn()),
            )],
        )
    }

    #[test]
    fn nested_template_preserves_custom_data_and_following_sentinel() {
        let mut bytes = Vec::new();
        serialize_item_stack_template(&custom_stack(), &mut bytes).expect("encode");
        bytes.push(0x7f);

        let expected = [
            0x98, 0x07, 0x02, 0x01, 0x00, 0x00, 0x0a, 0x03, 0x00, 0x01, 0x78, 0x00, 0x00, 0x00,
            0x07, 0x00, 0x7f,
        ];
        assert_eq!(bytes, expected);

        let mut input = bytes.as_slice();
        let decoded = deserialize_item_stack_template(&mut input).expect("decode");
        assert_eq!(decoded.item, &Item::BOWL);
        assert_eq!(decoded.item_count, 2);
        assert_eq!(
            decoded
                .get_data_component::<CustomDataImpl>()
                .expect("custom data")
                .data
                .get_int("x"),
            Some(7)
        );
        assert_eq!(input, &[0x7f]);
    }

    fn nested_use_remainder_wire(depth: usize) -> Vec<u8> {
        if depth == 0 {
            return vec![0x98, 0x07, 0x01, 0x00, 0x00];
        }
        let mut bytes = vec![0x98, 0x07, 0x01, 0x01, 0x00, 0x19];
        bytes.extend(nested_use_remainder_wire(depth - 1));
        bytes
    }

    #[test]
    fn nested_template_depth_has_checked_boundary() {
        let at_limit_bytes = nested_use_remainder_wire(63);
        let mut at_limit = at_limit_bytes.as_slice();
        assert!(deserialize_item_stack_template(&mut at_limit).is_ok());
        assert!(at_limit.is_empty());

        let over_limit_bytes = nested_use_remainder_wire(64);
        let mut over_limit = over_limit_bytes.as_slice();
        assert!(matches!(
            deserialize_item_stack_template(&mut over_limit),
            Err(ReadingError::TooLarge(_))
        ));
        let mut after_error = [0x98, 0x07, 0x01, 0x00, 0x00].as_slice();
        assert!(deserialize_item_stack_template(&mut after_error).is_ok());
    }

    #[test]
    fn use_remainder_wire_roundtrip_keeps_template() {
        let expected = UseRemainderImpl {
            convert_into: custom_stack(),
        };
        let mut encoded = Vec::new();
        expected.serialize(&mut encoded).expect("encode");
        let mut input = encoded.as_slice();
        let decoded = UseRemainderImpl::deserialize(&mut input).expect("decode");
        assert!(input.is_empty());
        assert_eq!(decoded, expected);
    }

    #[test]
    fn nested_template_rejects_empty_unknown_and_negative_values() {
        for bytes in [
            vec![0x00, 0x01, 0x00, 0x00],
            vec![0x8f, 0x4e, 0x01, 0x00, 0x00],
            vec![0x98, 0x07, 0x00, 0x00, 0x00],
            vec![0x98, 0x07, 0xff, 0xff, 0xff, 0xff, 0x0f, 0x00, 0x00],
            vec![0x98, 0x07, 0x01, 0x01, 0x00, 0x80, 0x01],
        ] {
            let mut input = bytes.as_slice();
            assert!(deserialize_item_stack_template(&mut input).is_err());
        }
    }

    #[test]
    fn stream_template_accepts_positive_count_above_nbt_range() {
        let mut input = [0x98, 0x07, 0x64, 0x00, 0x00].as_slice();
        let decoded = deserialize_item_stack_template(&mut input).expect("wire count 100");
        assert_eq!(decoded.item, &Item::BOWL);
        assert_eq!(decoded.item_count, 100);
        assert!(input.is_empty());
    }

    #[test]
    fn official_nested_component_shapes_have_independent_wire_fixtures() {
        let container = ContainerImpl {
            items: vec![(5, ItemStack::new(1, &Item::BOWL))],
        };
        let mut encoded = Vec::new();
        container.serialize(&mut encoded).expect("container encode");
        assert_eq!(encoded, [0x06, 0, 0, 0, 0, 0, 1, 0x98, 0x07, 1, 0, 0]);
        let decoded =
            ContainerImpl::deserialize(&mut encoded.as_slice()).expect("container decode");
        assert_eq!(decoded.items[0].0, 5);

        let charged = ChargedProjectilesImpl {
            projectiles: vec![ItemStack::new(1, &Item::ARROW)],
        };
        let mut encoded = Vec::new();
        charged.serialize(&mut encoded).expect("charged encode");
        assert_eq!(encoded, [1, 0x9b, 0x07, 1, 0, 0]);
        let decoded =
            ChargedProjectilesImpl::deserialize(&mut encoded.as_slice()).expect("charged decode");
        assert_eq!(decoded.projectiles[0].item, &Item::ARROW);

        let sulfur = SulfurCubeContentImpl {
            absorbed_block_item_stack: ItemStack::new(1, &Item::STONE),
        };
        let mut encoded = Vec::new();
        sulfur.serialize(&mut encoded).expect("sulfur encode");
        assert_eq!(encoded, [1, 1, 0, 0]);
        let decoded =
            SulfurCubeContentImpl::deserialize(&mut encoded.as_slice()).expect("sulfur decode");
        assert_eq!(decoded.absorbed_block_item_stack.item, &Item::STONE);
    }

    #[test]
    fn duplicate_patch_ids_are_last_write_wins_and_encode_once() {
        let mut stack = ItemStack::new(1, &Item::BOWL);
        stack
            .patch
            .push((DataComponent::Unbreakable, Some(UnbreakableImpl.to_dyn())));
        stack.patch.push((DataComponent::Unbreakable, None));
        let mut encoded = Vec::new();
        serialize_item_stack_template(&stack, &mut encoded).expect("encode");
        assert_eq!(encoded, [0x98, 0x07, 1, 0, 1, 4]);

        let mut input = [0x98, 0x07, 1, 1, 1, 4, 4].as_slice();
        let decoded = deserialize_item_stack_template(&mut input).expect("decode");
        assert!(!decoded.has_data_component(DataComponent::Unbreakable));
        assert!(input.is_empty());
    }

    #[test]
    fn generated_known_remainder_defaults_roundtrip() {
        for item in [
            &Item::MILK_BUCKET,
            &Item::HONEY_BOTTLE,
            &Item::MUSHROOM_STEW,
        ] {
            let (_, value) = item
                .components
                .iter()
                .find(|(id, _)| *id == DataComponent::UseRemainder)
                .expect("generated remainder");
            let remainder = super::get::<UseRemainderImpl>(*value);
            let mut bytes = Vec::new();
            remainder.serialize(&mut bytes).expect("encode");
            let mut input = bytes.as_slice();
            let decoded = UseRemainderImpl::deserialize(&mut input).expect("decode");
            assert!(input.is_empty());
            assert_eq!(decoded, *remainder);
        }
    }
}

impl DataComponentCodec<Self> for FireworkExplosionImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        // Shape (VarInt enum)
        seq.write_var_int(&VarInt::from(self.shape.to_id()))?;
        // Colors list
        seq.write_var_int(&VarInt::from(self.colors.len() as i32))?;
        for color in &self.colors {
            seq.write_i32(*color)?;
        }
        // Fade colors list
        seq.write_var_int(&VarInt::from(self.fade_colors.len() as i32))?;
        for color in &self.fade_colors {
            seq.write_i32(*color)?;
        }
        // hasTrail
        seq.write_bool(self.has_trail)?;
        // hasTwinkle
        seq.write_bool(self.has_twinkle)?;
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        // Needs a length cap during deserialization to prevent OOM from malicious packets
        // Vanilla doesn't have any limits (Integer.MAX_VALUE is technically a limit but not enforced in practice)
        const MAX_COLORS: usize = 256;
        const MAX_FADE_COLORS: usize = 256;

        // Shape (VarInt enum)
        let shape_id = seq.get_var_int()?.0;
        let shape = FireworkExplosionShape::from_id(shape_id).ok_or(ReadingError::Message(
            "Invalid FireworkExplosionShape id!".into(),
        ))?;

        // Colors list
        let colors_len = seq.get_var_int()?.0 as usize;
        if colors_len > MAX_COLORS {
            return Err(ReadingError::Message(format!(
                "FireworkExplosionImpl colors_len {colors_len} exceeds maximum of {MAX_COLORS}"
            )));
        }
        let mut colors = Vec::with_capacity(colors_len);
        for _ in 0..colors_len {
            let color = seq.get_i32()?;
            colors.push(color);
        }

        // Fade colors list
        let fade_colors_len = seq.get_var_int()?.0 as usize;
        if fade_colors_len > MAX_FADE_COLORS {
            return Err(ReadingError::Message(format!(
                "FireworkExplosionImpl fade_colors_len {fade_colors_len} exceeds maximum of {MAX_FADE_COLORS}"
            )));
        }
        let mut fade_colors = Vec::with_capacity(fade_colors_len);
        for _ in 0..fade_colors_len {
            let color = seq.get_i32()?;
            fade_colors.push(color);
        }

        // hasTrail
        let has_trail = seq.get_bool()?;

        // hasTwinkle
        let has_twinkle = seq.get_bool()?;

        Ok(Self::new(
            shape,
            colors,
            fade_colors,
            has_trail,
            has_twinkle,
        ))
    }
}

impl DataComponentCodec<Self> for FireworksImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        // Flight duration (VarInt)
        seq.write_var_int(&VarInt::from(self.flight_duration))?;
        // Explosions list
        seq.write_var_int(&VarInt::from(self.explosions.len() as i32))?;
        for explosion in &self.explosions {
            explosion.serialize(seq)?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        // Needs a length cap during deserialization to prevent OOM from malicious packets
        // Vanilla doesn't have any limits
        const MAX_EXPLOSIONS: usize = 256;
        // Vanilla restricts to 0-255 (UNSIGNED_BYTE in data component codec) (do not trust client NBT to limit it)
        const MAX_FLIGHT_DURATION: i32 = 255;

        // Flight duration
        let flight_duration = seq.get_var_int()?.0;
        if !(0..=MAX_FLIGHT_DURATION).contains(&flight_duration) {
            return Err(ReadingError::Message(format!(
                "FireworksImpl flight_duration {flight_duration} is out of bounds (0-{MAX_FLIGHT_DURATION})"
            )));
        }

        // Explosions list
        let explosions_len = seq.get_var_int()?.0 as usize;
        if explosions_len > MAX_EXPLOSIONS {
            return Err(ReadingError::Message(format!(
                "FireworksImpl explosions_len {explosions_len} exceeds maximum of {MAX_EXPLOSIONS}"
            )));
        }
        let mut explosions = Vec::with_capacity(explosions_len);
        for _ in 0..explosions_len {
            // Recursively deserialize each explosion
            let explosion = FireworkExplosionImpl::deserialize(seq)?;
            explosions.push(explosion);
        }

        Ok(Self::new(flight_duration, explosions))
    }
}

impl DataComponentCodec<Self> for StoredEnchantmentsImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.enchantment.len() as i32))?;
        for (enc, level) in self.enchantment.iter() {
            seq.write_var_int(&VarInt::from(enc.id))?;
            seq.write_var_int(&VarInt::from(*level))?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        const MAX_ENCHANTMENTS: usize = 256;

        let len = seq.get_var_int()?.0 as usize;

        if len > MAX_ENCHANTMENTS {
            return Err(ReadingError::Message("Too many enchantments".into()));
        }

        let mut stored_enchantments = Vec::with_capacity(len);
        for _ in 0..len {
            let id = seq.get_var_int()?.0 as u8;
            let level = seq.get_var_int()?.0;
            stored_enchantments.push((
                Enchantment::from_id(id).ok_or(ReadingError::Message(
                    "StoredEnchantmentsImpl Enchantment VarInt Incorrect!".into(),
                ))?,
                level,
            ));
        }
        Ok(Self {
            enchantment: Cow::from(stored_enchantments),
        })
    }
}

impl DataComponentCodec<Self> for RepairableImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        serialize_idset(&self.items, seq)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        Ok(Self {
            items: deserialize_idset(seq)?,
        })
    }
}

impl DataComponentCodec<Self> for SwingAnimationImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.animation_type.to_id()))?;
        seq.write_var_int(&VarInt::from(self.duration))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let type_id = seq.get_var_int()?.0;
        let animation_type = SwingAnimationType::from_id(type_id).ok_or_else(|| {
            ReadingError::Message(format!("Invalid SwingAnimationType id {type_id}"))
        })?;
        let duration = seq.get_var_int()?.0;
        Ok(Self {
            animation_type,
            duration,
        })
    }
}

impl DataComponentCodec<Self> for RarityImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.rarity.to_id()))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let id = seq.get_var_int()?.0;
        let rarity = Rarity::from_id(id)
            .ok_or_else(|| ReadingError::Message(format!("Invalid Rarity id {id}")))?;
        Ok(Self { rarity })
    }
}

#[allow(clippy::too_many_lines)]
pub fn deserialize(
    id: DataComponent,
    seq: &mut impl NetworkReadExt,
) -> Result<Box<dyn DataComponentImpl>, ReadingError> {
    match id {
        DataComponent::CustomData => Ok(CustomDataImpl::deserialize(seq)?.to_dyn()),
        DataComponent::MaxStackSize => Ok(MaxStackSizeImpl::deserialize(seq)?.to_dyn()),
        DataComponent::MaxDamage => Ok(MaxDamageImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Damage => Ok(DamageImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Unbreakable => Ok(UnbreakableImpl::deserialize(seq)?.to_dyn()),
        DataComponent::UseEffects => Ok(UseEffectsImpl::deserialize(seq)?.to_dyn()),
        DataComponent::CustomName => Ok(CustomNameImpl::deserialize(seq)?.to_dyn()),
        DataComponent::MinimumAttackCharge => {
            Ok(MinimumAttackChargeImpl::deserialize(seq)?.to_dyn())
        }
        DataComponent::DamageType => Ok(DamageTypeImpl::deserialize(seq)?.to_dyn()),
        DataComponent::ItemName => Ok(ItemNameImpl::deserialize(seq)?.to_dyn()),
        DataComponent::ItemModel => Ok(ItemModelImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Lore => Ok(LoreImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Rarity => Ok(RarityImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Enchantments => Ok(EnchantmentsImpl::deserialize(seq)?.to_dyn()),
        DataComponent::CanPlaceOn => Ok(CanPlaceOnImpl::deserialize(seq)?.to_dyn()),
        DataComponent::CanBreak => Ok(CanBreakImpl::deserialize(seq)?.to_dyn()),
        DataComponent::AttributeModifiers => Ok(AttributeModifiersImpl::deserialize(seq)?.to_dyn()),
        DataComponent::CustomModelData => Ok(CustomModelDataImpl::deserialize(seq)?.to_dyn()),
        DataComponent::TooltipDisplay => Ok(TooltipDisplayImpl::deserialize(seq)?.to_dyn()),
        DataComponent::RepairCost => Ok(RepairCostImpl::deserialize(seq)?.to_dyn()),
        DataComponent::CreativeSlotLock => Ok(CreativeSlotLockImpl::deserialize(seq)?.to_dyn()),
        DataComponent::EnchantmentGlintOverride => {
            Ok(EnchantmentGlintOverrideImpl::deserialize(seq)?.to_dyn())
        }
        DataComponent::IntangibleProjectile => {
            Ok(IntangibleProjectileImpl::deserialize(seq)?.to_dyn())
        }
        DataComponent::Food => Ok(FoodImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Consumable => Ok(ConsumableImpl::deserialize(seq)?.to_dyn()),
        DataComponent::UseRemainder => Ok(UseRemainderImpl::deserialize(seq)?.to_dyn()),
        DataComponent::UseCooldown => Ok(UseCooldownImpl::deserialize(seq)?.to_dyn()),
        DataComponent::DamageResistant => Ok(DamageResistantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Tool => Ok(ToolImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Weapon => Ok(WeaponImpl::deserialize(seq)?.to_dyn()),
        DataComponent::AttackRange => Ok(AttackRangeImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Enchantable => Ok(EnchantableImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Equippable => Ok(EquippableImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Repairable => Ok(RepairableImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Glider => Ok(GliderImpl::deserialize(seq)?.to_dyn()),
        DataComponent::TooltipStyle => Ok(TooltipStyleImpl::deserialize(seq)?.to_dyn()),
        DataComponent::DeathProtection => Ok(DeathProtectionImpl::deserialize(seq)?.to_dyn()),
        DataComponent::BlocksAttacks => Ok(BlocksAttacksImpl::deserialize(seq)?.to_dyn()),
        DataComponent::PiercingWeapon => Ok(PiercingWeaponImpl::deserialize(seq)?.to_dyn()),
        DataComponent::KineticWeapon => Ok(KineticWeaponImpl::deserialize(seq)?.to_dyn()),
        DataComponent::SwingAnimation => Ok(SwingAnimationImpl::deserialize(seq)?.to_dyn()),
        DataComponent::AdditionalTradeCost => {
            Ok(AdditionalTradeCostImpl::deserialize(seq)?.to_dyn())
        }
        DataComponent::StoredEnchantments => Ok(StoredEnchantmentsImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Dye => Ok(DyeImpl::deserialize(seq)?.to_dyn()),
        DataComponent::DyedColor => Ok(DyedColorImpl::deserialize(seq)?.to_dyn()),
        DataComponent::MapColor => Ok(MapColorImpl::deserialize(seq)?.to_dyn()),
        DataComponent::MapId => Ok(MapIdImpl::deserialize(seq)?.to_dyn()),
        DataComponent::MapDecorations => Ok(MapDecorationsImpl::deserialize(seq)?.to_dyn()),
        DataComponent::MapPostProcessing => Ok(MapPostProcessingImpl::deserialize(seq)?.to_dyn()),
        DataComponent::ChargedProjectiles => Ok(ChargedProjectilesImpl::deserialize(seq)?.to_dyn()),
        DataComponent::BundleContents => Ok(BundleContentsImpl::deserialize(seq)?.to_dyn()),
        DataComponent::PotionContents => Ok(PotionContentsImpl::deserialize(seq)?.to_dyn()),
        DataComponent::PotionDurationScale => {
            Ok(PotionDurationScaleImpl::deserialize(seq)?.to_dyn())
        }
        DataComponent::SuspiciousStewEffects => {
            Ok(SuspiciousStewEffectsImpl::deserialize(seq)?.to_dyn())
        }
        DataComponent::WritableBookContent => {
            Ok(WritableBookContentImpl::deserialize(seq)?.to_dyn())
        }
        DataComponent::WrittenBookContent => Ok(WrittenBookContentImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Trim => Ok(TrimImpl::deserialize(seq)?.to_dyn()),
        DataComponent::DebugStickState => Ok(DebugStickStateImpl::deserialize(seq)?.to_dyn()),
        DataComponent::EntityData => Ok(EntityDataImpl::deserialize(seq)?.to_dyn()),
        DataComponent::BucketEntityData => Ok(BucketEntityDataImpl::deserialize(seq)?.to_dyn()),
        DataComponent::BlockEntityData => Ok(BlockEntityDataImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Instrument => Ok(InstrumentImpl::deserialize(seq)?.to_dyn()),
        DataComponent::ProvidesTrimMaterial => {
            Ok(ProvidesTrimMaterialImpl::deserialize(seq)?.to_dyn())
        }
        DataComponent::OminousBottleAmplifier => {
            Ok(OminousBottleAmplifierImpl::deserialize(seq)?.to_dyn())
        }
        DataComponent::JukeboxPlayable => Ok(JukeboxPlayableImpl::deserialize(seq)?.to_dyn()),
        DataComponent::ProvidesBannerPatterns => {
            Ok(ProvidesBannerPatternsImpl::deserialize(seq)?.to_dyn())
        }
        DataComponent::Recipes => Ok(RecipesImpl::deserialize(seq)?.to_dyn()),
        DataComponent::LodestoneTracker => Ok(LodestoneTrackerImpl::deserialize(seq)?.to_dyn()),
        DataComponent::FireworkExplosion => Ok(FireworkExplosionImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Fireworks => Ok(FireworksImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Profile => Ok(ProfileImpl::deserialize(seq)?.to_dyn()),
        DataComponent::NoteBlockSound => Ok(NoteBlockSoundImpl::deserialize(seq)?.to_dyn()),
        DataComponent::BannerPatterns => Ok(BannerPatternsImpl::deserialize(seq)?.to_dyn()),
        DataComponent::BaseColor => Ok(BaseColorImpl::deserialize(seq)?.to_dyn()),
        DataComponent::PotDecorations => Ok(PotDecorationsImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Container => Ok(ContainerImpl::deserialize(seq)?.to_dyn()),
        DataComponent::BlockState => Ok(BlockStateImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Bees => Ok(BeesImpl::deserialize(seq)?.to_dyn()),
        DataComponent::SulfurCubeContent => Ok(SulfurCubeContentImpl::deserialize(seq)?.to_dyn()),
        DataComponent::Lock => Ok(LockImpl::deserialize(seq)?.to_dyn()),
        DataComponent::ContainerLoot => Ok(ContainerLootImpl::deserialize(seq)?.to_dyn()),
        DataComponent::BreakSound => Ok(BreakSoundImpl::deserialize(seq)?.to_dyn()),
        DataComponent::VillagerVariant => Ok(VillagerVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::WolfVariant => Ok(WolfVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::WolfSoundVariant => Ok(WolfSoundVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::WolfCollar => Ok(WolfCollarImpl::deserialize(seq)?.to_dyn()),
        DataComponent::FoxVariant => Ok(FoxVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::SalmonSize => Ok(SalmonSizeImpl::deserialize(seq)?.to_dyn()),
        DataComponent::ParrotVariant => Ok(ParrotVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::TropicalFishPattern => {
            Ok(TropicalFishPatternImpl::deserialize(seq)?.to_dyn())
        }
        DataComponent::TropicalFishBaseColor => {
            Ok(TropicalFishBaseColorImpl::deserialize(seq)?.to_dyn())
        }
        DataComponent::TropicalFishPatternColor => {
            Ok(TropicalFishPatternColorImpl::deserialize(seq)?.to_dyn())
        }
        DataComponent::MooshroomVariant => Ok(MooshroomVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::RabbitVariant => Ok(RabbitVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::PigVariant => Ok(PigVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::PigSoundVariant => Ok(PigSoundVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::CowVariant => Ok(CowVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::CowSoundVariant => Ok(CowSoundVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::ChickenVariant => Ok(ChickenVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::ChickenSoundVariant => {
            Ok(ChickenSoundVariantImpl::deserialize(seq)?.to_dyn())
        }
        DataComponent::ZombieNautilusVariant => {
            Ok(ZombieNautilusVariantImpl::deserialize(seq)?.to_dyn())
        }
        DataComponent::FrogVariant => Ok(FrogVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::HorseVariant => Ok(HorseVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::PaintingVariant => Ok(PaintingVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::LlamaVariant => Ok(LlamaVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::AxolotlVariant => Ok(AxolotlVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::CatVariant => Ok(CatVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::CatSoundVariant => Ok(CatSoundVariantImpl::deserialize(seq)?.to_dyn()),
        DataComponent::CatCollar => Ok(CatCollarImpl::deserialize(seq)?.to_dyn()),
        DataComponent::SheepColor => Ok(SheepColorImpl::deserialize(seq)?.to_dyn()),
        DataComponent::ShulkerColor => Ok(ShulkerColorImpl::deserialize(seq)?.to_dyn()),
    }
}

#[allow(clippy::too_many_lines)]
pub fn serialize(
    id: DataComponent,
    value: &dyn DataComponentImpl,
    seq: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    match id {
        DataComponent::CustomData => get::<CustomDataImpl>(value).serialize(seq),
        DataComponent::MaxStackSize => get::<MaxStackSizeImpl>(value).serialize(seq),
        DataComponent::MaxDamage => get::<MaxDamageImpl>(value).serialize(seq),
        DataComponent::Damage => get::<DamageImpl>(value).serialize(seq),
        DataComponent::Unbreakable => get::<UnbreakableImpl>(value).serialize(seq),
        DataComponent::UseEffects => get::<UseEffectsImpl>(value).serialize(seq),
        DataComponent::CustomName => get::<CustomNameImpl>(value).serialize(seq),
        DataComponent::MinimumAttackCharge => get::<MinimumAttackChargeImpl>(value).serialize(seq),
        DataComponent::DamageType => get::<DamageTypeImpl>(value).serialize(seq),
        DataComponent::ItemName => get::<ItemNameImpl>(value).serialize(seq),
        DataComponent::ItemModel => get::<ItemModelImpl>(value).serialize(seq),
        DataComponent::Lore => get::<LoreImpl>(value).serialize(seq),
        DataComponent::Rarity => get::<RarityImpl>(value).serialize(seq),
        DataComponent::Enchantments => get::<EnchantmentsImpl>(value).serialize(seq),
        DataComponent::CanPlaceOn => get::<CanPlaceOnImpl>(value).serialize(seq),
        DataComponent::CanBreak => get::<CanBreakImpl>(value).serialize(seq),
        DataComponent::AttributeModifiers => get::<AttributeModifiersImpl>(value).serialize(seq),
        DataComponent::CustomModelData => get::<CustomModelDataImpl>(value).serialize(seq),
        DataComponent::TooltipDisplay => get::<TooltipDisplayImpl>(value).serialize(seq),
        DataComponent::RepairCost => get::<RepairCostImpl>(value).serialize(seq),
        DataComponent::CreativeSlotLock => get::<CreativeSlotLockImpl>(value).serialize(seq),
        DataComponent::EnchantmentGlintOverride => {
            get::<EnchantmentGlintOverrideImpl>(value).serialize(seq)
        }
        DataComponent::IntangibleProjectile => {
            get::<IntangibleProjectileImpl>(value).serialize(seq)
        }
        DataComponent::Food => get::<FoodImpl>(value).serialize(seq),
        DataComponent::Consumable => get::<ConsumableImpl>(value).serialize(seq),
        DataComponent::UseRemainder => get::<UseRemainderImpl>(value).serialize(seq),
        DataComponent::UseCooldown => get::<UseCooldownImpl>(value).serialize(seq),
        DataComponent::DamageResistant => get::<DamageResistantImpl>(value).serialize(seq),
        DataComponent::Tool => get::<ToolImpl>(value).serialize(seq),
        DataComponent::Weapon => get::<WeaponImpl>(value).serialize(seq),
        DataComponent::AttackRange => get::<AttackRangeImpl>(value).serialize(seq),
        DataComponent::Enchantable => get::<EnchantableImpl>(value).serialize(seq),
        DataComponent::Equippable => get::<EquippableImpl>(value).serialize(seq),
        DataComponent::Repairable => get::<RepairableImpl>(value).serialize(seq),
        DataComponent::Glider => get::<GliderImpl>(value).serialize(seq),
        DataComponent::TooltipStyle => get::<TooltipStyleImpl>(value).serialize(seq),
        DataComponent::DeathProtection => get::<DeathProtectionImpl>(value).serialize(seq),
        DataComponent::BlocksAttacks => get::<BlocksAttacksImpl>(value).serialize(seq),
        DataComponent::PiercingWeapon => get::<PiercingWeaponImpl>(value).serialize(seq),
        DataComponent::KineticWeapon => get::<KineticWeaponImpl>(value).serialize(seq),
        DataComponent::SwingAnimation => get::<SwingAnimationImpl>(value).serialize(seq),
        DataComponent::AdditionalTradeCost => get::<AdditionalTradeCostImpl>(value).serialize(seq),
        DataComponent::StoredEnchantments => get::<StoredEnchantmentsImpl>(value).serialize(seq),
        DataComponent::Dye => get::<DyeImpl>(value).serialize(seq),
        DataComponent::DyedColor => get::<DyedColorImpl>(value).serialize(seq),
        DataComponent::MapColor => get::<MapColorImpl>(value).serialize(seq),
        DataComponent::MapId => get::<MapIdImpl>(value).serialize(seq),
        DataComponent::MapDecorations => get::<MapDecorationsImpl>(value).serialize(seq),
        DataComponent::MapPostProcessing => get::<MapPostProcessingImpl>(value).serialize(seq),
        DataComponent::ChargedProjectiles => get::<ChargedProjectilesImpl>(value).serialize(seq),
        DataComponent::BundleContents => get::<BundleContentsImpl>(value).serialize(seq),
        DataComponent::PotionContents => get::<PotionContentsImpl>(value).serialize(seq),
        DataComponent::PotionDurationScale => get::<PotionDurationScaleImpl>(value).serialize(seq),
        DataComponent::SuspiciousStewEffects => {
            get::<SuspiciousStewEffectsImpl>(value).serialize(seq)
        }
        DataComponent::WritableBookContent => get::<WritableBookContentImpl>(value).serialize(seq),
        DataComponent::WrittenBookContent => get::<WrittenBookContentImpl>(value).serialize(seq),
        DataComponent::Trim => get::<TrimImpl>(value).serialize(seq),
        DataComponent::DebugStickState => get::<DebugStickStateImpl>(value).serialize(seq),
        DataComponent::EntityData => get::<EntityDataImpl>(value).serialize(seq),
        DataComponent::BucketEntityData => get::<BucketEntityDataImpl>(value).serialize(seq),
        DataComponent::BlockEntityData => get::<BlockEntityDataImpl>(value).serialize(seq),
        DataComponent::Instrument => get::<InstrumentImpl>(value).serialize(seq),
        DataComponent::ProvidesTrimMaterial => {
            get::<ProvidesTrimMaterialImpl>(value).serialize(seq)
        }
        DataComponent::OminousBottleAmplifier => {
            get::<OminousBottleAmplifierImpl>(value).serialize(seq)
        }
        DataComponent::JukeboxPlayable => get::<JukeboxPlayableImpl>(value).serialize(seq),
        DataComponent::ProvidesBannerPatterns => {
            get::<ProvidesBannerPatternsImpl>(value).serialize(seq)
        }
        DataComponent::Recipes => get::<RecipesImpl>(value).serialize(seq),
        DataComponent::LodestoneTracker => get::<LodestoneTrackerImpl>(value).serialize(seq),
        DataComponent::FireworkExplosion => get::<FireworkExplosionImpl>(value).serialize(seq),
        DataComponent::Fireworks => get::<FireworksImpl>(value).serialize(seq),
        DataComponent::Profile => get::<ProfileImpl>(value).serialize(seq),
        DataComponent::NoteBlockSound => get::<NoteBlockSoundImpl>(value).serialize(seq),
        DataComponent::BannerPatterns => get::<BannerPatternsImpl>(value).serialize(seq),
        DataComponent::BaseColor => get::<BaseColorImpl>(value).serialize(seq),
        DataComponent::PotDecorations => get::<PotDecorationsImpl>(value).serialize(seq),
        DataComponent::Container => get::<ContainerImpl>(value).serialize(seq),
        DataComponent::BlockState => get::<BlockStateImpl>(value).serialize(seq),
        DataComponent::Bees => get::<BeesImpl>(value).serialize(seq),
        DataComponent::SulfurCubeContent => get::<SulfurCubeContentImpl>(value).serialize(seq),
        DataComponent::Lock => get::<LockImpl>(value).serialize(seq),
        DataComponent::ContainerLoot => get::<ContainerLootImpl>(value).serialize(seq),
        DataComponent::BreakSound => get::<BreakSoundImpl>(value).serialize(seq),
        DataComponent::VillagerVariant => get::<VillagerVariantImpl>(value).serialize(seq),
        DataComponent::WolfVariant => get::<WolfVariantImpl>(value).serialize(seq),
        DataComponent::WolfSoundVariant => get::<WolfSoundVariantImpl>(value).serialize(seq),
        DataComponent::WolfCollar => get::<WolfCollarImpl>(value).serialize(seq),
        DataComponent::FoxVariant => get::<FoxVariantImpl>(value).serialize(seq),
        DataComponent::SalmonSize => get::<SalmonSizeImpl>(value).serialize(seq),
        DataComponent::ParrotVariant => get::<ParrotVariantImpl>(value).serialize(seq),
        DataComponent::TropicalFishPattern => get::<TropicalFishPatternImpl>(value).serialize(seq),
        DataComponent::TropicalFishBaseColor => {
            get::<TropicalFishBaseColorImpl>(value).serialize(seq)
        }
        DataComponent::TropicalFishPatternColor => {
            get::<TropicalFishPatternColorImpl>(value).serialize(seq)
        }
        DataComponent::MooshroomVariant => get::<MooshroomVariantImpl>(value).serialize(seq),
        DataComponent::RabbitVariant => get::<RabbitVariantImpl>(value).serialize(seq),
        DataComponent::PigVariant => get::<PigVariantImpl>(value).serialize(seq),
        DataComponent::PigSoundVariant => get::<PigSoundVariantImpl>(value).serialize(seq),
        DataComponent::CowVariant => get::<CowVariantImpl>(value).serialize(seq),
        DataComponent::CowSoundVariant => get::<CowSoundVariantImpl>(value).serialize(seq),
        DataComponent::ChickenVariant => get::<ChickenVariantImpl>(value).serialize(seq),
        DataComponent::ChickenSoundVariant => get::<ChickenSoundVariantImpl>(value).serialize(seq),
        DataComponent::ZombieNautilusVariant => {
            get::<ZombieNautilusVariantImpl>(value).serialize(seq)
        }
        DataComponent::FrogVariant => get::<FrogVariantImpl>(value).serialize(seq),
        DataComponent::HorseVariant => get::<HorseVariantImpl>(value).serialize(seq),
        DataComponent::PaintingVariant => get::<PaintingVariantImpl>(value).serialize(seq),
        DataComponent::LlamaVariant => get::<LlamaVariantImpl>(value).serialize(seq),
        DataComponent::AxolotlVariant => get::<AxolotlVariantImpl>(value).serialize(seq),
        DataComponent::CatVariant => get::<CatVariantImpl>(value).serialize(seq),
        DataComponent::CatSoundVariant => get::<CatSoundVariantImpl>(value).serialize(seq),
        DataComponent::CatCollar => get::<CatCollarImpl>(value).serialize(seq),
        DataComponent::SheepColor => get::<SheepColorImpl>(value).serialize(seq),
        DataComponent::ShulkerColor => get::<ShulkerColorImpl>(value).serialize(seq),
    }
}

impl DataComponentCodec<Self> for MapIdImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.id))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let id = seq.get_var_int()?.0;
        Ok(Self { id })
    }
}

impl DataComponentCodec<Self> for UseCooldownImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_f32(self.seconds)?;
        seq.write_bool(self.cooldown_group.is_some())?;
        if let Some(group) = &self.cooldown_group {
            seq.write_string(group)?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let seconds = seq.get_f32()?;
        let cooldown_group = if seq.get_bool()? {
            Some(seq.get_str()?.into())
        } else {
            None
        };
        Ok(Self {
            seconds,
            cooldown_group,
        })
    }
}

fn deserialize_item_stack_template(
    seq: &mut impl NetworkReadExt,
) -> Result<pumpkin_data::item_stack::ItemStack, ReadingError> {
    const MAX_COMPONENTS: i32 = 256;
    let _depth = ItemStackTemplateDepth::enter()
        .map_err(|()| ReadingError::TooLarge("ItemStackTemplate nesting exceeded".into()))?;

    let item_id = u16::try_from(seq.get_var_int()?.0)
        .map_err(|_| ReadingError::Message("Invalid item ID in ItemStackTemplate".into()))?;
    let item = pumpkin_data::item::Item::from_id(item_id)
        .filter(|item| item.id != pumpkin_data::item::Item::AIR.id)
        .ok_or_else(|| {
            ReadingError::Message("Unknown or empty item ID in ItemStackTemplate".into())
        })?;

    // The NBT codec has a 1..=99 range; STREAM_CODEC carries the raw
    // positive count. ItemStack stores u8, so values above 255 remain an
    // explicit Pumpkin representation ceiling rather than an official reject.
    let count = u8::try_from(seq.get_var_int()?.0)
        .ok()
        .filter(|count| *count > 0)
        .ok_or_else(|| ReadingError::Message("Invalid ItemStackTemplate count".into()))?;

    let num_to_add = seq.get_var_int()?.0;
    let num_to_remove = seq.get_var_int()?.0;

    if num_to_add < 0 || num_to_remove < 0 {
        return Err(ReadingError::Message("Negative component count".into()));
    }

    let total_components = num_to_add
        .checked_add(num_to_remove)
        .ok_or_else(|| ReadingError::Message("Component count overflow".into()))?;

    if total_components > MAX_COMPONENTS {
        return Err(ReadingError::Message(
            "Too many components in ItemStackTemplate patch".into(),
        ));
    }

    let mut patch = Vec::with_capacity((num_to_add + num_to_remove) as usize);

    for _ in 0..num_to_add {
        let id_val = seq.get_var_int()?.0;
        let id = u8::try_from(id_val)
            .ok()
            .and_then(DataComponent::try_from_id)
            .ok_or_else(|| ReadingError::Message(format!("Unknown component ID: {id_val}")))?;

        let component_impl = deserialize(id, seq)?;
        if let Some((_, value)) = patch.iter_mut().find(|(patch_id, _)| *patch_id == id) {
            *value = Some(component_impl);
        } else {
            patch.push((id, Some(component_impl)));
        }
    }

    for _ in 0..num_to_remove {
        let id_val = seq.get_var_int()?.0;
        let id = u8::try_from(id_val)
            .ok()
            .and_then(DataComponent::try_from_id)
            .ok_or_else(|| ReadingError::Message("Unknown component ID".into()))?;
        if let Some((_, value)) = patch.iter_mut().find(|(patch_id, _)| *patch_id == id) {
            *value = None;
        } else {
            patch.push((id, None));
        }
    }

    Ok(pumpkin_data::item_stack::ItemStack::new_with_component(
        count, item, patch,
    ))
}

fn serialize_item_stack_template(
    stack: &pumpkin_data::item_stack::ItemStack,
    seq: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    let _depth = ItemStackTemplateDepth::enter()
        .map_err(|()| WritingError::Message("ItemStackTemplate nesting exceeded".into()))?;
    if stack.item.id == pumpkin_data::item::Item::AIR.id || stack.item_count == 0 {
        return Err(WritingError::Message(
            "Invalid ItemStackTemplate item/count".into(),
        ));
    }

    seq.write_var_int(&VarInt::from(stack.item.id))?;
    seq.write_var_int(&VarInt::from(stack.item_count))?;

    let effective = stack
        .patch
        .iter()
        .enumerate()
        .filter(|(index, (id, _))| {
            !stack.patch[index + 1..]
                .iter()
                .any(|(later_id, _)| later_id == id)
        })
        .map(|(_, entry)| entry)
        .collect::<Vec<_>>();
    let to_add = effective.iter().filter(|(_, data)| data.is_some()).count();
    let to_remove = effective.iter().filter(|(_, data)| data.is_none()).count();
    if to_add + to_remove > 256 {
        return Err(WritingError::Message(
            "Too many ItemStackTemplate components".into(),
        ));
    }

    seq.write_var_int(&VarInt::from(
        i32::try_from(to_add).map_err(|_| WritingError::Message("Too many components".into()))?,
    ))?;
    seq.write_var_int(&VarInt::from(
        i32::try_from(to_remove)
            .map_err(|_| WritingError::Message("Too many components".into()))?,
    ))?;

    for (id, data) in &effective {
        if let Some(data) = data {
            seq.write_var_int(&VarInt::from(id.to_id()))?;
            serialize(*id, data.as_ref(), seq)?;
        }
    }

    for (id, data) in &effective {
        if data.is_none() {
            seq.write_var_int(&VarInt::from(id.to_id()))?;
        }
    }

    Ok(())
}

impl DataComponentCodec<Self> for BundleContentsImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        // Pumpkin defense ceiling; the official generic list codec has no
        // component-specific 64-item maximum.
        const MAX_BUNDLE_ITEMS: usize = 64;
        if self.items.len() > MAX_BUNDLE_ITEMS {
            return Err(WritingError::Message(
                "Too many BundleContents items for Pumpkin limit".into(),
            ));
        }
        seq.write_var_int(&VarInt::from(i32::try_from(self.items.len()).map_err(
            |_| WritingError::Message("Too many BundleContents items for Pumpkin limit".into()),
        )?))?;
        for item in &self.items {
            serialize_item_stack_template(item, seq)?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        // Pumpkin defense ceiling; this is not an official BundleContents
        // wire maximum.
        const MAX_BUNDLE_ITEMS: usize = 64;

        let len = usize::try_from(seq.get_var_int()?.0)
            .map_err(|_| ReadingError::Message("Negative BundleContents count".into()))?;

        if len > MAX_BUNDLE_ITEMS {
            return Err(ReadingError::Message(
                "Too many BundleContents items for Pumpkin limit".into(),
            ));
        }

        let mut items = Vec::with_capacity(len);
        for _ in 0..len {
            items.push(deserialize_item_stack_template(seq)?);
        }
        Ok(Self { items })
    }
}

macro_rules! codec_string_variant {
    ($struct_name:ident) => {
        impl DataComponentCodec<Self> for $struct_name {
            fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
                seq.write_string(&self.value)
            }
            fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
                let value = seq.get_str()?;
                Ok(Self {
                    value: Cow::Owned(value.into()),
                })
            }
        }
    };
}

codec_string_variant!(VillagerVariantImpl);
codec_string_variant!(WolfVariantImpl);
codec_string_variant!(WolfSoundVariantImpl);
codec_string_variant!(WolfCollarImpl);
codec_string_variant!(FoxVariantImpl);
codec_string_variant!(SalmonSizeImpl);
codec_string_variant!(ParrotVariantImpl);
codec_string_variant!(TropicalFishPatternImpl);
codec_string_variant!(TropicalFishBaseColorImpl);
codec_string_variant!(TropicalFishPatternColorImpl);
codec_string_variant!(MooshroomVariantImpl);
codec_string_variant!(RabbitVariantImpl);
codec_string_variant!(PigVariantImpl);
codec_string_variant!(PigSoundVariantImpl);
codec_string_variant!(CowVariantImpl);
codec_string_variant!(CowSoundVariantImpl);
codec_string_variant!(ChickenVariantImpl);
codec_string_variant!(ChickenSoundVariantImpl);
codec_string_variant!(ZombieNautilusVariantImpl);
codec_string_variant!(FrogVariantImpl);
codec_string_variant!(HorseVariantImpl);
codec_string_variant!(PaintingVariantImpl);
codec_string_variant!(LlamaVariantImpl);
codec_string_variant!(AxolotlVariantImpl);
codec_string_variant!(CatVariantImpl);
codec_string_variant!(CatSoundVariantImpl);
codec_string_variant!(CatCollarImpl);
codec_string_variant!(SheepColorImpl);
codec_string_variant!(ShulkerColorImpl);

impl DataComponentCodec<Self> for MaxDamageImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.max_damage))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let max_damage = seq.get_var_int()?.0;
        Ok(Self { max_damage })
    }
}

impl DataComponentCodec<Self> for UseEffectsImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_bool(false)?;
        seq.write_bool(true)?;
        seq.write_f32(0.2)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let _can_sprint = seq.get_bool()?;
        let _interact_vibrations = seq.get_bool()?;
        let _speed_multiplier = seq.get_f32()?;
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for MinimumAttackChargeImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_f32(self.charge)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let charge = seq.get_f32()?;
        Ok(Self { charge })
    }
}

impl DataComponentCodec<Self> for DamageTypeImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.damage_type.id as i32))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let id = seq.get_var_int()?.0 as u8;
        let damage_type = pumpkin_data::damage::DamageType::from_id(id)
            .ok_or_else(|| ReadingError::Message(format!("Invalid DamageType id {id}")))?;
        Ok(Self { damage_type })
    }
}

const MAX_ADVENTURE_PREDICATES: usize = 256;
const MAX_ADVENTURE_BLOCKS: usize = 256;
const MAX_ADVENTURE_PROPERTIES: usize = 256;
const MAX_ADVENTURE_COMPONENTS: usize = 256;

fn adventure_count(
    seq: &mut impl NetworkReadExt,
    what: &str,
    max: usize,
) -> Result<usize, ReadingError> {
    let value = seq.get_var_int()?.0;
    let count = usize::try_from(value)
        .map_err(|_| ReadingError::Message(format!("Negative {what} count: {value}")))?;
    if count > max {
        return Err(ReadingError::TooLarge(format!(
            "{what} count {count} exceeds {max}"
        )));
    }
    Ok(count)
}
fn adventure_component(value: i32) -> Result<DataComponent, ReadingError> {
    let id = u8::try_from(value)
        .map_err(|_| ReadingError::Message(format!("Invalid data component id: {value}")))?;
    DataComponent::try_from_id(id)
        .ok_or_else(|| ReadingError::Message(format!("Unknown data component id: {value}")))
}
fn adventure_component_name(name: &str) -> Result<DataComponent, WritingError> {
    DataComponent::try_from_name(name).ok_or_else(|| {
        WritingError::Message(format!(
            "Unknown data component in adventure predicate: {name}"
        ))
    })
}
fn read_adventure_blocks(seq: &mut impl NetworkReadExt) -> Result<NbtTag, ReadingError> {
    match seq.get_var_int()?.0 {
        0 => Ok(NbtTag::String(format!("#{}", seq.get_str()?).into())),
        value if value > 0 => {
            let count = usize::try_from(value - 1)
                .map_err(|_| ReadingError::Message("Invalid adventure block set length".into()))?;
            if count > MAX_ADVENTURE_BLOCKS {
                return Err(ReadingError::TooLarge(
                    "Too many adventure block IDs".into(),
                ));
            }
            let mut blocks = Vec::with_capacity(count);
            for _ in 0..count {
                let id = u16::try_from(seq.get_var_int()?.0)
                    .map_err(|_| ReadingError::Message("Invalid adventure block ID".into()))?;
                let block = BlockId::new(id).map(Block::from_id).ok_or_else(|| {
                    ReadingError::Message(format!("Unknown adventure block registry ID: {id}"))
                })?;
                blocks.push(NbtTag::String(block.name.into()));
            }
            Ok(NbtTag::List(blocks))
        }
        _ => Err(ReadingError::Message(
            "Negative adventure block set type/length".into(),
        )),
    }
}
fn write_adventure_blocks(
    blocks: &NbtTag,
    seq: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    match blocks {
        NbtTag::String(name) if name.starts_with('#') => {
            seq.write_var_int(&VarInt(0))?;
            seq.write_string(name.strip_prefix('#').unwrap_or(name))
        }
        NbtTag::String(name) => {
            let block = Block::from_name(name)
                .ok_or_else(|| WritingError::Message(format!("Unknown adventure block: {name}")))?;
            seq.write_var_int(&VarInt(2))?;
            seq.write_var_int(&VarInt(i32::from(block.registry_id())))
        }
        NbtTag::List(names) => {
            if names.len() > MAX_ADVENTURE_BLOCKS {
                return Err(WritingError::Message("Too many adventure block IDs".into()));
            }
            seq.write_var_int(&VarInt(i32::try_from(names.len() + 1).map_err(|_| {
                WritingError::Message("Adventure block count overflow".into())
            })?))?;
            for name in names {
                let NbtTag::String(name) = name else {
                    return Err(WritingError::Message(
                        "Adventure block ID must be a string".into(),
                    ));
                };
                let block = Block::from_name(name).ok_or_else(|| {
                    WritingError::Message(format!("Unknown adventure block: {name}"))
                })?;
                seq.write_var_int(&VarInt(i32::from(block.registry_id())))?;
            }
            Ok(())
        }
        _ => Err(WritingError::Message(
            "Adventure blocks must be a string or list".into(),
        )),
    }
}
fn read_adventure_state(seq: &mut impl NetworkReadExt) -> Result<NbtTag, ReadingError> {
    let count = adventure_count(seq, "adventure state property", MAX_ADVENTURE_PROPERTIES)?;
    let mut state = NbtCompound::new();
    for _ in 0..count {
        let name = seq.get_str()?;
        if state.get(&name).is_some() {
            return Err(ReadingError::Message(format!(
                "Duplicate adventure state property: {name}"
            )));
        }
        if seq.get_bool()? {
            state.put(&name, NbtTag::String(seq.get_str()?));
        } else {
            let mut range = NbtCompound::new();
            if seq.get_bool()? {
                range.put_string("min", seq.get_str()?.into());
            }
            if seq.get_bool()? {
                range.put_string("max", seq.get_str()?.into());
            }
            state.put(&name, NbtTag::Compound(range));
        }
    }
    Ok(NbtTag::Compound(state))
}
fn write_adventure_state(
    state: &NbtTag,
    seq: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    let NbtTag::Compound(state) = state else {
        return Err(WritingError::Message(
            "Adventure state must be a compound".into(),
        ));
    };
    if state.child_tags.len() > MAX_ADVENTURE_PROPERTIES {
        return Err(WritingError::Message(
            "Too many adventure state properties".into(),
        ));
    }
    seq.write_var_int(&VarInt(i32::try_from(state.child_tags.len()).map_err(
        |_| WritingError::Message("Adventure state count overflow".into()),
    )?))?;
    for (name, value) in &state.child_tags {
        seq.write_string(name)?;
        match value {
            NbtTag::String(value) => {
                seq.write_bool(true)?;
                seq.write_string(value)?;
            }
            NbtTag::Compound(range) => {
                if range
                    .child_tags
                    .keys()
                    .any(|key| key.as_ref() != "min" && key.as_ref() != "max")
                {
                    return Err(WritingError::Message(format!(
                        "Unknown adventure state range field: {name}"
                    )));
                }
                seq.write_bool(false)?;
                let min = range.get_string("min");
                seq.write_bool(min.is_some())?;
                if let Some(min) = min {
                    seq.write_string(min)?;
                }
                let max = range.get_string("max");
                seq.write_bool(max.is_some())?;
                if let Some(max) = max {
                    seq.write_string(max)?;
                }
            }
            _ => {
                return Err(WritingError::Message(format!(
                    "Invalid adventure state value: {name}"
                )));
            }
        }
    }
    Ok(())
}
fn read_adventure_components(seq: &mut impl NetworkReadExt) -> Result<NbtTag, ReadingError> {
    let exact_count = adventure_count(seq, "adventure exact component", MAX_ADVENTURE_COMPONENTS)?;
    let mut exact = NbtCompound::new();
    for _ in 0..exact_count {
        let id = adventure_component(seq.get_var_int()?.0)?;
        let name = id.to_name();
        if exact.get(name).is_some() {
            return Err(ReadingError::Message(format!(
                "Duplicate adventure exact component: {name}"
            )));
        }
        let value = if id == DataComponent::TooltipDisplay {
            let mut tooltip = NbtCompound::new();
            tooltip.put_bool("hide_tooltip", seq.get_bool()?);
            let count = adventure_count(seq, "tooltip hidden component", MAX_ADVENTURE_COMPONENTS)?;
            let mut hidden = Vec::with_capacity(count);
            for _ in 0..count {
                hidden.push(NbtTag::String(
                    adventure_component(seq.get_var_int()?.0)?.to_name().into(),
                ));
            }
            tooltip.put_list("hidden_components", hidden);
            NbtTag::Compound(tooltip)
        } else {
            deserialize(id, seq)?.write_data()
        };
        exact.put(name, value);
    }
    let partial_count =
        adventure_count(seq, "adventure partial component", MAX_ADVENTURE_COMPONENTS)?;
    let mut partial = Vec::with_capacity(partial_count);
    for _ in 0..partial_count {
        partial.push(NbtTag::String(
            adventure_component(seq.get_var_int()?.0)?.to_name().into(),
        ));
    }
    let mut result = NbtCompound::new();
    if !exact.child_tags.is_empty() {
        result.put("components", NbtTag::Compound(exact));
    }
    if !partial.is_empty() {
        result.put_list("predicates", partial);
    }
    Ok(NbtTag::Compound(result))
}
fn write_adventure_tooltip(
    tooltip: &NbtCompound,
    seq: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    let hidden = tooltip.get_list("hidden_components").ok_or_else(|| {
        WritingError::Message("TooltipDisplay matcher is missing hidden_components".into())
    })?;
    let hide = tooltip.get_bool("hide_tooltip").ok_or_else(|| {
        WritingError::Message("TooltipDisplay matcher is missing hide_tooltip".into())
    })?;
    if tooltip
        .child_tags
        .keys()
        .any(|key| key.as_ref() != "hide_tooltip" && key.as_ref() != "hidden_components")
    {
        return Err(WritingError::Message(
            "Unknown TooltipDisplay matcher field".into(),
        ));
    }
    seq.write_bool(hide)?;
    seq.write_var_int(&VarInt(i32::try_from(hidden.len()).map_err(|_| {
        WritingError::Message("TooltipDisplay hidden component count overflow".into())
    })?))?;
    for item in hidden {
        let NbtTag::String(name) = item else {
            return Err(WritingError::Message(
                "TooltipDisplay hidden component ID must be a string".into(),
            ));
        };
        seq.write_var_int(&VarInt(i32::from(adventure_component_name(name)?.to_id())))?;
    }
    Ok(())
}
fn write_adventure_components(
    value: &NbtTag,
    seq: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    let NbtTag::Compound(value) = value else {
        return Err(WritingError::Message(
            "Adventure components must be a compound".into(),
        ));
    };
    let exact = match value.get("components") {
        None => None,
        Some(NbtTag::Compound(value)) => Some(value),
        Some(_) => {
            return Err(WritingError::Message(
                "Adventure exact components must be a compound".into(),
            ));
        }
    };
    let partial = match value.get("predicates") {
        None => None,
        Some(NbtTag::List(value)) => Some(value),
        Some(_) => {
            return Err(WritingError::Message(
                "Adventure partial components must be a list".into(),
            ));
        }
    };
    if value
        .child_tags
        .keys()
        .any(|key| key.as_ref() != "components" && key.as_ref() != "predicates")
    {
        return Err(WritingError::Message(
            "Unknown adventure component matcher field".into(),
        ));
    }
    let exact_len = exact.map_or(0, |value| value.child_tags.len());
    if exact_len > MAX_ADVENTURE_COMPONENTS {
        return Err(WritingError::Message(
            "Too many adventure exact components".into(),
        ));
    }
    seq.write_var_int(&VarInt(i32::try_from(exact_len).map_err(|_| {
        WritingError::Message("Adventure exact component count overflow".into())
    })?))?;
    if let Some(exact) = exact {
        for (name, component_value) in &exact.child_tags {
            let id = adventure_component_name(name)?;
            seq.write_var_int(&VarInt(i32::from(id.to_id())))?;
            if id == DataComponent::TooltipDisplay
                && let NbtTag::Compound(tooltip) = component_value
            {
                write_adventure_tooltip(tooltip, seq)?;
                continue;
            }
            let component = pumpkin_data::data_component_impl::read_data(id, component_value)
                .ok_or_else(|| {
                    WritingError::Message(format!("Invalid data for component {name}"))
                })?;
            serialize(id, component.as_ref(), seq)?;
        }
    }
    let partial_len = partial.map_or(0, Vec::len);
    if partial_len > MAX_ADVENTURE_COMPONENTS {
        return Err(WritingError::Message(
            "Too many adventure partial components".into(),
        ));
    }
    seq.write_var_int(&VarInt(i32::try_from(partial_len).map_err(|_| {
        WritingError::Message("Adventure partial component count overflow".into())
    })?))?;
    if let Some(partial) = partial {
        for item in partial {
            let NbtTag::String(name) = item else {
                return Err(WritingError::Message(
                    "Adventure partial component ID must be a string".into(),
                ));
            };
            seq.write_var_int(&VarInt(i32::from(adventure_component_name(name)?.to_id())))?;
        }
    }
    Ok(())
}
fn read_adventure_predicate(seq: &mut impl NetworkReadExt) -> Result<NbtTag, ReadingError> {
    let blocks = if seq.get_bool()? {
        Some(read_adventure_blocks(seq)?)
    } else {
        None
    };
    let state = if seq.get_bool()? {
        Some(read_adventure_state(seq)?)
    } else {
        None
    };
    let nbt = if seq.get_bool()? {
        Some(NbtTag::Compound(
            seq.get_compound_nbt_with_version(&JavaMinecraftVersion::V_26_2)?
                .ok_or_else(|| {
                    ReadingError::Message("Adventure predicate NBT is missing".into())
                })?,
        ))
    } else {
        None
    };
    let components = read_adventure_components(seq);
    let mut result = NbtCompound::new();
    if let Some(value) = blocks {
        result.put("blocks", value);
    }
    if let Some(value) = state {
        result.put("state", value);
    }
    if let Some(value) = nbt {
        result.put("nbt", value);
    }
    if let NbtTag::Compound(value) = components?
        && !value.child_tags.is_empty()
    {
        result.put("components", NbtTag::Compound(value));
    }
    Ok(NbtTag::Compound(result))
}
fn write_adventure_predicate(
    value: &NbtCompound,
    seq: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    if value.child_tags.keys().any(|key| {
        key.as_ref() != "blocks"
            && key.as_ref() != "state"
            && key.as_ref() != "nbt"
            && key.as_ref() != "components"
    }) {
        return Err(WritingError::Message(
            "Unknown adventure predicate field".into(),
        ));
    }
    let blocks = value.get("blocks");
    seq.write_bool(blocks.is_some())?;
    if let Some(blocks) = blocks {
        write_adventure_blocks(blocks, seq)?;
    }
    let state = value.get("state");
    seq.write_bool(state.is_some())?;
    if let Some(state) = state {
        write_adventure_state(state, seq)?;
    }
    let nbt = value.get("nbt");
    seq.write_bool(nbt.is_some())?;
    if let Some(NbtTag::Compound(nbt)) = nbt {
        seq.write_nbt_with_version(
            Some(&NbtTag::Compound(nbt.clone())),
            &JavaMinecraftVersion::V_26_2,
        )?;
    } else if nbt.is_some() {
        return Err(WritingError::Message(
            "Adventure predicate NBT must be a compound".into(),
        ));
    }
    if let Some(components) = value.get("components") {
        write_adventure_components(components, seq)
    } else {
        seq.write_var_int(&VarInt(0))?;
        seq.write_var_int(&VarInt(0))
    }
}
fn read_adventure_predicates(seq: &mut impl NetworkReadExt) -> Result<NbtTag, ReadingError> {
    let count = adventure_count(seq, "adventure predicate", MAX_ADVENTURE_PREDICATES)?;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(read_adventure_predicate(seq)?);
    }
    Ok(NbtTag::List(values))
}
fn write_adventure_predicates(
    value: &NbtTag,
    seq: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    match value {
        NbtTag::List(values) => {
            if values.len() > MAX_ADVENTURE_PREDICATES {
                return Err(WritingError::Message(
                    "Too many adventure predicates".into(),
                ));
            }
            seq.write_var_int(&VarInt(i32::try_from(values.len()).map_err(|_| {
                WritingError::Message("Adventure predicate count overflow".into())
            })?))?;
            for value in values {
                let NbtTag::Compound(value) = value else {
                    return Err(WritingError::Message(
                        "Adventure predicate must be a compound".into(),
                    ));
                };
                write_adventure_predicate(value, seq)?;
            }
            Ok(())
        }
        NbtTag::Compound(value) => {
            seq.write_var_int(&VarInt(1))?;
            write_adventure_predicate(value, seq)
        }
        _ => Err(WritingError::Message(
            "Adventure predicate must be a compound or list".into(),
        )),
    }
}
impl DataComponentCodec<Self> for CanPlaceOnImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        write_adventure_predicates(&self.predicate, seq)
    }
    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        Ok(Self {
            predicate: read_adventure_predicates(seq)?,
        })
    }
}
impl DataComponentCodec<Self> for CanBreakImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        write_adventure_predicates(&self.predicate, seq)
    }
    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        Ok(Self {
            predicate: read_adventure_predicates(seq)?,
        })
    }
}

const fn attribute_modifier_slot_id(
    slot: &pumpkin_data::enchantment::AttributeModifierSlot,
) -> i32 {
    match slot {
        pumpkin_data::enchantment::AttributeModifierSlot::Any => 0,
        pumpkin_data::enchantment::AttributeModifierSlot::MainHand => 1,
        pumpkin_data::enchantment::AttributeModifierSlot::OffHand => 2,
        pumpkin_data::enchantment::AttributeModifierSlot::Hand => 3,
        pumpkin_data::enchantment::AttributeModifierSlot::Feet => 4,
        pumpkin_data::enchantment::AttributeModifierSlot::Legs => 5,
        pumpkin_data::enchantment::AttributeModifierSlot::Chest => 6,
        pumpkin_data::enchantment::AttributeModifierSlot::Head => 7,
        pumpkin_data::enchantment::AttributeModifierSlot::Armor => 8,
        pumpkin_data::enchantment::AttributeModifierSlot::Body => 9,
        pumpkin_data::enchantment::AttributeModifierSlot::Saddle => 10,
    }
}

fn attribute_modifier_slot(
    value: i32,
) -> Result<pumpkin_data::enchantment::AttributeModifierSlot, ReadingError> {
    match value {
        0 => Ok(pumpkin_data::enchantment::AttributeModifierSlot::Any),
        1 => Ok(pumpkin_data::enchantment::AttributeModifierSlot::MainHand),
        2 => Ok(pumpkin_data::enchantment::AttributeModifierSlot::OffHand),
        3 => Ok(pumpkin_data::enchantment::AttributeModifierSlot::Hand),
        4 => Ok(pumpkin_data::enchantment::AttributeModifierSlot::Feet),
        5 => Ok(pumpkin_data::enchantment::AttributeModifierSlot::Legs),
        6 => Ok(pumpkin_data::enchantment::AttributeModifierSlot::Chest),
        7 => Ok(pumpkin_data::enchantment::AttributeModifierSlot::Head),
        8 => Ok(pumpkin_data::enchantment::AttributeModifierSlot::Armor),
        9 => Ok(pumpkin_data::enchantment::AttributeModifierSlot::Body),
        10 => Ok(pumpkin_data::enchantment::AttributeModifierSlot::Saddle),
        _ => Err(ReadingError::Message(format!(
            "Invalid attribute modifier slot id: {value}"
        ))),
    }
}

fn attribute_modifier_operation(
    value: i32,
) -> Result<pumpkin_data::data_component_impl::Operation, ReadingError> {
    match value {
        0 => Ok(pumpkin_data::data_component_impl::Operation::AddValue),
        1 => Ok(pumpkin_data::data_component_impl::Operation::AddMultipliedBase),
        2 => Ok(pumpkin_data::data_component_impl::Operation::AddMultipliedTotal),
        _ => Err(ReadingError::Message(format!(
            "Invalid attribute modifier operation id: {value}"
        ))),
    }
}

fn serialize_modifier_display(
    display: &ModifierDisplay,
    seq: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    match display {
        ModifierDisplay::Default => seq.write_var_int(&VarInt(0)),
        ModifierDisplay::Hidden => seq.write_var_int(&VarInt(1)),
        ModifierDisplay::Override(component) => {
            if matches!(component, NbtTag::End) {
                return Err(WritingError::Message(
                    "Attribute modifier display component cannot be End".into(),
                ));
            }
            seq.write_var_int(&VarInt(2))?;
            seq.write_nbt_with_version(Some(component), &JavaMinecraftVersion::V_26_2)
        }
    }
}

fn deserialize_modifier_display(
    seq: &mut impl NetworkReadExt,
) -> Result<ModifierDisplay, ReadingError> {
    match seq.get_var_int()?.0 {
        0 => Ok(ModifierDisplay::Default),
        1 => Ok(ModifierDisplay::Hidden),
        2 => Ok(ModifierDisplay::Override(
            seq.get_nbt_with_version(&JavaMinecraftVersion::V_26_2)?
                .ok_or_else(|| {
                    ReadingError::Message("Attribute modifier display component is missing".into())
                })?,
        )),
        value => Err(ReadingError::Message(format!(
            "Invalid attribute modifier display type id: {value}"
        ))),
    }
}

impl DataComponentCodec<Self> for AttributeModifiersImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        let count = i32::try_from(self.attribute_modifiers.len())
            .map_err(|_| WritingError::Message("Too many attribute modifiers".into()))?;
        seq.write_var_int(&VarInt(count))?;
        for modifier in self.attribute_modifiers.iter() {
            seq.write_var_int(&VarInt(i32::from(modifier.r#type.id)))?;
            Identifier::parse(&modifier.id).map_err(|error| {
                WritingError::Message(format!("Invalid attribute modifier resource id: {error}"))
            })?;
            seq.write_string(&modifier.id)?;
            seq.write_f64(modifier.amount)?;
            let operation = match modifier.operation {
                Operation::AddValue => 0,
                Operation::AddMultipliedBase => 1,
                Operation::AddMultipliedTotal => 2,
            };
            seq.write_var_int(&VarInt(operation))?;
            seq.write_var_int(&VarInt(attribute_modifier_slot_id(&modifier.slot)))?;
            serialize_modifier_display(&modifier.display, seq)?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let count = seq.get_var_int()?.0;
        let count = usize::try_from(count).map_err(|_| {
            ReadingError::Message(format!("Negative attribute modifier count: {count}"))
        })?;
        let mut modifiers = Vec::new();
        for _ in 0..count {
            let attribute_id = seq.get_var_int()?.0;
            let attribute_id = usize::try_from(attribute_id).map_err(|_| {
                ReadingError::Message(format!("Invalid attribute registry id: {attribute_id}"))
            })?;
            let attribute = pumpkin_data::attributes::Attributes::ALL
                .get(attribute_id)
                .ok_or_else(|| {
                    ReadingError::Message(format!("Unknown attribute registry id: {attribute_id}"))
                })?;
            let id = seq.get_str()?;
            Identifier::parse(&id).map_err(|error| {
                ReadingError::Message(format!("Invalid attribute modifier resource id: {error}"))
            })?;
            let amount = seq.get_f64()?;
            let operation = attribute_modifier_operation(seq.get_var_int()?.0)?;
            let slot = attribute_modifier_slot(seq.get_var_int()?.0)?;
            let display = deserialize_modifier_display(seq)?;
            modifiers.push(Modifier {
                r#type: attribute,
                id: Cow::Owned(id.into()),
                amount,
                operation,
                slot,
                display,
            });
        }
        Ok(Self {
            attribute_modifiers: Cow::Owned(modifiers),
        })
    }
}

impl DataComponentCodec<Self> for CustomModelDataImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.floats.len() as i32))?;
        for f in &self.floats {
            seq.write_f32(*f)?;
        }
        seq.write_var_int(&VarInt::from(self.flags.len() as i32))?;
        for b in &self.flags {
            seq.write_bool(*b)?;
        }
        seq.write_var_int(&VarInt::from(self.strings.len() as i32))?;
        for s in &self.strings {
            seq.write_string(s)?;
        }
        seq.write_var_int(&VarInt::from(self.colors.len() as i32))?;
        for c in &self.colors {
            seq.write_i32(*c)?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let floats_len = seq.get_var_int()?.0 as usize;
        let mut floats = Vec::with_capacity(floats_len);
        for _ in 0..floats_len {
            floats.push(seq.get_f32()?);
        }
        let flags_len = seq.get_var_int()?.0 as usize;
        let mut flags = Vec::with_capacity(flags_len);
        for _ in 0..flags_len {
            flags.push(seq.get_bool()?);
        }
        let strings_len = seq.get_var_int()?.0 as usize;
        let mut strings = Vec::with_capacity(strings_len);
        for _ in 0..strings_len {
            strings.push(seq.get_str()?.to_string());
        }
        let colors_len = seq.get_var_int()?.0 as usize;
        let mut colors = Vec::with_capacity(colors_len);
        for _ in 0..colors_len {
            colors.push(seq.get_i32()?);
        }
        Ok(Self {
            floats,
            flags,
            strings,
            colors,
        })
    }
}

impl DataComponentCodec<Self> for TooltipDisplayImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        if self.hidden_components.len() > MAX_TOOLTIP_HIDDEN_COMPONENTS {
            return Err(WritingError::Message(
                "Too many hidden tooltip components".into(),
            ));
        }
        seq.write_bool(self.hide_tooltip)?;
        seq.write_var_int(&VarInt(
            i32::try_from(self.hidden_components.len()).map_err(|_| {
                WritingError::Message("Hidden tooltip component count overflow".into())
            })?,
        ))?;
        for id in &self.hidden_components {
            seq.write_var_int(&VarInt(i32::from(id.to_id())))?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let hide_tooltip = seq.get_bool()?;
        let count = usize::try_from(seq.get_var_int()?.0)
            .map_err(|_| ReadingError::Message("Negative hidden tooltip component count".into()))?;
        if count > MAX_TOOLTIP_HIDDEN_COMPONENTS {
            return Err(ReadingError::Message(
                "Too many hidden tooltip components".into(),
            ));
        }
        let mut hidden_components = Vec::with_capacity(count);
        for _ in 0..count {
            let id = u8::try_from(seq.get_var_int()?.0)
                .map_err(|_| ReadingError::Message("Invalid hidden tooltip component ID".into()))?;
            let id = DataComponent::try_from_id(id).ok_or_else(|| {
                ReadingError::Message("Unknown hidden tooltip component ID".into())
            })?;
            if !hidden_components.contains(&id) {
                hidden_components.push(id);
            }
        }
        Ok(Self {
            hide_tooltip,
            hidden_components,
        })
    }
}

impl DataComponentCodec<Self> for CreativeSlotLockImpl {
    fn serialize(&self, _seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        Ok(())
    }

    fn deserialize(_seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for EnchantmentGlintOverrideImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_bool(true)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let _ = seq.get_bool()?;
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for IntangibleProjectileImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        serialize_nbt_fallback(self, seq)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        deserialize_nbt_fallback(seq, "intangible_projectile", Self::read_data)
    }
}

impl DataComponentCodec<Self> for FoodImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.nutrition))?;
        seq.write_f32(self.saturation)?;
        seq.write_bool(self.can_always_eat)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let nutrition = seq.get_var_int()?.0;
        let saturation = seq.get_f32()?;
        let can_always_eat = seq.get_bool()?;
        Ok(Self {
            nutrition,
            saturation,
            can_always_eat,
        })
    }
}

impl DataComponentCodec<Self> for UseRemainderImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        serialize_item_stack_template(&self.convert_into, seq)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        Ok(Self {
            convert_into: deserialize_item_stack_template(seq)?,
        })
    }
}

impl DataComponentCodec<Self> for DamageResistantImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_string(self.res_type.as_str())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let tag = seq.get_str()?;
        Ok(Self {
            res_type: DamageResistantType::from_tag(&tag),
        })
    }
}

impl DataComponentCodec<Self> for ToolImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.rules.len() as i32))?;
        for rule in self.rules.iter() {
            serialize_idset(&rule.blocks, seq)?;
            seq.write_bool(rule.speed.is_some())?;
            if let Some(speed) = rule.speed {
                seq.write_f32(speed)?;
            }
            seq.write_bool(rule.correct_for_drops.is_some())?;
            if let Some(correct) = rule.correct_for_drops {
                seq.write_bool(correct)?;
            }
        }
        seq.write_f32(self.default_mining_speed)?;
        seq.write_var_int(&VarInt::from(self.damage_per_block as i32))?;
        seq.write_bool(self.can_destroy_blocks_in_creative)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let rules_len = seq.get_var_int()?.0 as usize;
        let mut rules = Vec::with_capacity(rules_len);
        for _ in 0..rules_len {
            let blocks = deserialize_idset(seq)?;
            let speed = if seq.get_bool()? {
                Some(seq.get_f32()?)
            } else {
                None
            };
            let correct_for_drops = if seq.get_bool()? {
                Some(seq.get_bool()?)
            } else {
                None
            };
            rules.push(pumpkin_data::data_component_impl::ToolRule {
                blocks,
                speed,
                correct_for_drops,
            });
        }
        let default_mining_speed = seq.get_f32()?;
        let damage_per_block = seq.get_var_int()?.0 as u32;
        let can_destroy_blocks_in_creative = seq.get_bool()?;
        Ok(Self {
            rules: Cow::Owned(rules),
            default_mining_speed,
            damage_per_block,
            can_destroy_blocks_in_creative,
        })
    }
}

impl DataComponentCodec<Self> for WeaponImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.item_damage_per_attack as i32))?;
        seq.write_f32(0.0)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let item_damage_per_attack = seq.get_var_int()?.0 as u32;
        let _disable_blocking_for_seconds = seq.get_f32()?;
        Ok(Self {
            item_damage_per_attack,
        })
    }
}

impl DataComponentCodec<Self> for AttackRangeImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_f32(self.min_reach)?;
        seq.write_f32(self.max_reach)?;
        seq.write_f32(self.min_creative_reach)?;
        seq.write_f32(self.max_creative_reach)?;
        seq.write_f32(self.hitbox_margin)?;
        seq.write_f32(self.mob_factor)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let min_reach = seq.get_f32()?;
        let max_reach = seq.get_f32()?;
        let min_creative_reach = seq.get_f32()?;
        let max_creative_reach = seq.get_f32()?;
        let hitbox_margin = seq.get_f32()?;
        let mob_factor = seq.get_f32()?;
        Ok(Self {
            min_reach,
            max_reach,
            min_creative_reach,
            max_creative_reach,
            hitbox_margin,
            mob_factor,
        })
    }
}

impl DataComponentCodec<Self> for EnchantableImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.value))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let value = seq.get_var_int()?.0;
        Ok(Self { value })
    }
}

impl DataComponentCodec<Self> for GliderImpl {
    fn serialize(&self, _seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        Ok(())
    }

    fn deserialize(_seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for TooltipStyleImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_string(&self.id)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let id = seq.get_str()?.to_string();
        Ok(Self { id })
    }
}

impl DataComponentCodec<Self> for DeathProtectionImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        if self.death_effects.len() > MAX_DEATH_EFFECTS {
            return Err(WritingError::Message("Too many death effects".into()));
        }
        let count = i32::try_from(self.death_effects.len())
            .map_err(|_| WritingError::Message("Too many death effects".into()))?;
        seq.write_var_int(&VarInt(count))?;
        for effect in self.death_effects.iter() {
            serialize_consume_effect(effect, seq)?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let count = seq.get_var_int()?.0;
        if count < 0 || count as usize > MAX_DEATH_EFFECTS {
            return Err(ReadingError::Message("Invalid death effect count".into()));
        }

        let mut death_effects = Vec::with_capacity(count as usize);
        for _ in 0..count {
            death_effects.push(deserialize_consume_effect(seq)?);
        }
        Ok(Self {
            death_effects: Cow::Owned(death_effects),
        })
    }
}

impl DataComponentCodec<Self> for BlocksAttacksImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_f32(0.0)?;
        seq.write_f32(1.0)?;
        seq.write_var_int(&VarInt(0))?;
        seq.write_var_int(&VarInt(0))?;
        seq.write_bool(false)?;
        seq.write_bool(false)?;
        seq.write_bool(false)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let _block_delay = seq.get_f32()?;
        let _disable_scale = seq.get_f32()?;
        let red_len = seq.get_var_int()?.0 as usize;
        for _ in 0..red_len {
            let _ = seq.get_f32()?;
            if seq.get_bool()? {
                let id_type = seq.get_var_int()?.0;
                if id_type == 0 {
                    let _ = seq.get_str()?;
                } else if id_type > 0 {
                    for _ in 0..(id_type - 1) {
                        let _ = seq.get_var_int()?;
                    }
                }
            }
            let _ = seq.get_f32()?;
            let _ = seq.get_f32()?;
        }
        let item_damage_type = seq.get_var_int()?.0;
        if item_damage_type == 1 {
            let _ = seq.get_f32()?;
            let _ = seq.get_f32()?;
        }
        if seq.get_bool()? {
            let id_type = seq.get_var_int()?.0;
            if id_type == 0 {
                let _ = seq.get_str()?;
            } else if id_type > 0 {
                for _ in 0..(id_type - 1) {
                    let _ = seq.get_var_int()?;
                }
            }
        }
        if seq.get_bool()? {
            let _ = seq.get_var_int()?;
        }
        if seq.get_bool()? {
            let _ = seq.get_var_int()?;
        }
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for PiercingWeaponImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_bool(self.deals_knockback)?;
        seq.write_bool(self.dismounts)?;
        if let Some(sound) = &self.sound {
            seq.write_bool(true)?;
            let proto_sound = data_to_proto_sound(sound);
            crate::IdOr::<crate::SoundEvent>::write(&proto_sound, seq, |w, e| {
                w.write_string(&e.sound_name)?;
                w.write_option(&e.range, |w2, r| w2.write_f32(*r))
            })?;
        } else {
            seq.write_bool(false)?;
        }
        if let Some(sound) = &self.hit_sound {
            seq.write_bool(true)?;
            let proto_sound = data_to_proto_sound(sound);
            crate::IdOr::<crate::SoundEvent>::write(&proto_sound, seq, |w, e| {
                w.write_string(&e.sound_name)?;
                w.write_option(&e.range, |w2, r| w2.write_f32(*r))
            })?;
        } else {
            seq.write_bool(false)?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let deals_knockback = seq.get_bool()?;
        let dismounts = seq.get_bool()?;
        let sound = if seq.get_bool()? {
            let proto = crate::IdOr::<crate::SoundEvent>::read(seq, |r| {
                let sound_name = r.get_str()?.to_string();
                let range = r.get_option(NetworkReadExt::get_f32)?;
                Ok(crate::SoundEvent { sound_name, range })
            })
            .map_err(|e| ReadingError::Message(format!("No sound: {e}")))?;
            proto_to_data_sound(&proto)
        } else {
            None
        };
        let hit_sound = if seq.get_bool()? {
            let proto = crate::IdOr::<crate::SoundEvent>::read(seq, |r| {
                let sound_name = r.get_str()?.to_string();
                let range = r.get_option(NetworkReadExt::get_f32)?;
                Ok(crate::SoundEvent { sound_name, range })
            })
            .map_err(|e| ReadingError::Message(format!("No sound: {e}")))?;
            proto_to_data_sound(&proto)
        } else {
            None
        };
        Ok(Self {
            deals_knockback,
            dismounts,
            sound,
            hit_sound,
        })
    }
}

fn serialize_kinetic_condition(
    cond: &pumpkin_data::data_component_impl::KineticConditionImpl,
    seq: &mut impl NetworkWriteExt,
) -> Result<(), WritingError> {
    seq.write_var_int(&VarInt::from(cond.max_duration_ticks))?;
    seq.write_f32(cond.min_speed)?;
    seq.write_f32(cond.min_relative_speed)
}

fn deserialize_kinetic_condition(
    seq: &mut impl NetworkReadExt,
) -> Result<pumpkin_data::data_component_impl::KineticConditionImpl, ReadingError> {
    let max_duration_ticks = seq.get_var_int()?.0;
    let min_speed = seq.get_f32()?;
    let min_relative_speed = seq.get_f32()?;
    Ok(pumpkin_data::data_component_impl::KineticConditionImpl {
        max_duration_ticks,
        min_speed,
        min_relative_speed,
    })
}

impl DataComponentCodec<Self> for KineticWeaponImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.contact_cooldown_ticks))?;
        seq.write_var_int(&VarInt::from(self.delay_ticks))?;
        if let Some(cond) = &self.dismount_conditions {
            seq.write_bool(true)?;
            serialize_kinetic_condition(cond, seq)?;
        } else {
            seq.write_bool(false)?;
        }
        if let Some(cond) = &self.knockback_conditions {
            seq.write_bool(true)?;
            serialize_kinetic_condition(cond, seq)?;
        } else {
            seq.write_bool(false)?;
        }
        if let Some(cond) = &self.damage_conditions {
            seq.write_bool(true)?;
            serialize_kinetic_condition(cond, seq)?;
        } else {
            seq.write_bool(false)?;
        }
        seq.write_f32(self.forward_movement)?;
        seq.write_f32(self.damage_multiplier)?;
        if let Some(sound) = &self.sound {
            seq.write_bool(true)?;
            let proto = data_to_proto_sound(sound);
            crate::IdOr::<crate::SoundEvent>::write(&proto, seq, |w, e| {
                w.write_string(&e.sound_name)?;
                w.write_option(&e.range, |w2, r| w2.write_f32(*r))
            })?;
        } else {
            seq.write_bool(false)?;
        }
        if let Some(sound) = &self.hit_sound {
            seq.write_bool(true)?;
            let proto = data_to_proto_sound(sound);
            crate::IdOr::<crate::SoundEvent>::write(&proto, seq, |w, e| {
                w.write_string(&e.sound_name)?;
                w.write_option(&e.range, |w2, r| w2.write_f32(*r))
            })?;
        } else {
            seq.write_bool(false)?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let contact_cooldown_ticks = seq.get_var_int()?.0;
        let delay_ticks = seq.get_var_int()?.0;
        let dismount_conditions = if seq.get_bool()? {
            Some(deserialize_kinetic_condition(seq)?)
        } else {
            None
        };
        let knockback_conditions = if seq.get_bool()? {
            Some(deserialize_kinetic_condition(seq)?)
        } else {
            None
        };
        let damage_conditions = if seq.get_bool()? {
            Some(deserialize_kinetic_condition(seq)?)
        } else {
            None
        };
        let forward_movement = seq.get_f32()?;
        let damage_multiplier = seq.get_f32()?;
        let sound = if seq.get_bool()? {
            let proto = crate::IdOr::<crate::SoundEvent>::read(seq, |r| {
                let sound_name = r.get_str()?.to_string();
                let range = r.get_option(NetworkReadExt::get_f32)?;
                Ok(crate::SoundEvent { sound_name, range })
            })
            .map_err(|e| ReadingError::Message(format!("No sound: {e}")))?;
            proto_to_data_sound(&proto)
        } else {
            None
        };
        let hit_sound = if seq.get_bool()? {
            let proto = crate::IdOr::<crate::SoundEvent>::read(seq, |r| {
                let sound_name = r.get_str()?.to_string();
                let range = r.get_option(NetworkReadExt::get_f32)?;
                Ok(crate::SoundEvent { sound_name, range })
            })
            .map_err(|e| ReadingError::Message(format!("No sound: {e}")))?;
            proto_to_data_sound(&proto)
        } else {
            None
        };
        Ok(Self {
            contact_cooldown_ticks,
            delay_ticks,
            dismount_conditions,
            knockback_conditions,
            damage_conditions,
            forward_movement,
            damage_multiplier,
            sound,
            hit_sound,
        })
    }
}

impl DataComponentCodec<Self> for AdditionalTradeCostImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt(0))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let _ = seq.get_var_int()?;
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for DyeImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt(0))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let _ = seq.get_var_int()?;
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for MapColorImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_i32(0)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let _ = seq.get_i32()?;
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for MapDecorationsImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        serialize_nbt_fallback(self, seq)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        deserialize_nbt_fallback(seq, "map_decorations", Self::read_data)
    }
}

impl DataComponentCodec<Self> for MapPostProcessingImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.processing.map_or(0, |p| p as i32)))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let val = seq.get_var_int()?.0;
        let processing = match val {
            0 => Some(pumpkin_data::data_component_impl::MapPostProcessing::Lock),
            1 => Some(pumpkin_data::data_component_impl::MapPostProcessing::Scale),
            _ => None,
        };
        Ok(Self { processing })
    }
}

impl DataComponentCodec<Self> for ChargedProjectilesImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        const MAX_CHARGED_PROJECTILES: usize = 1024;
        if self.projectiles.len() > MAX_CHARGED_PROJECTILES {
            return Err(WritingError::Message(
                "Too many ChargedProjectiles items for official limit".into(),
            ));
        }
        seq.write_var_int(&VarInt::from(
            i32::try_from(self.projectiles.len())
                .map_err(|_| WritingError::Message("Too many ChargedProjectiles items".into()))?,
        ))?;
        for item in &self.projectiles {
            serialize_item_stack_template(item, seq)?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        const MAX_CHARGED_PROJECTILES: usize = 1024;
        let len = usize::try_from(seq.get_var_int()?.0)
            .map_err(|_| ReadingError::Message("Negative ChargedProjectiles count".into()))?;
        if len > MAX_CHARGED_PROJECTILES {
            return Err(ReadingError::TooLarge(
                "Too many ChargedProjectiles items for official limit".into(),
            ));
        }
        let mut projectiles = Vec::with_capacity(len);
        for _ in 0..len {
            projectiles.push(deserialize_item_stack_template(seq)?);
        }
        Ok(Self { projectiles })
    }
}

impl DataComponentCodec<Self> for PotionDurationScaleImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_f32(self.scale)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let scale = seq.get_f32()?;
        Ok(Self { scale })
    }
}

impl DataComponentCodec<Self> for WritableBookContentImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.pages.len() as i32))?;
        for page in &self.pages {
            seq.write_string(page)?;
            seq.write_bool(false)?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let len = seq.get_var_int()?.0 as usize;
        let mut pages = Vec::with_capacity(len);
        for _ in 0..len {
            let raw = seq.get_str()?.to_string();
            let has_filtered = seq.get_bool()?;
            if has_filtered {
                let _ = seq.get_str()?;
            }
            pages.push(raw);
        }
        Ok(Self { pages })
    }
}

impl DataComponentCodec<Self> for WrittenBookContentImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_string(&self.title)?;
        seq.write_bool(false)?;
        seq.write_string(&self.author)?;
        seq.write_var_int(&VarInt(0))?;
        seq.write_var_int(&VarInt::from(self.pages.len() as i32))?;
        for page in &self.pages {
            let comp = pumpkin_util::text::TextComponent::text(page.clone());
            seq.write_slice(&comp.encode_for_version(&JavaMinecraftVersion::V_26_2))?;
            seq.write_bool(false)?;
        }
        seq.write_bool(true)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let title = seq.get_str()?.to_string();
        if seq.get_bool()? {
            let _ = seq.get_str()?;
        }
        let author = seq.get_str()?.to_string();
        let _generation = seq.get_var_int()?.0;
        let pages_len = seq.get_var_int()?.0 as usize;
        let mut pages = Vec::with_capacity(pages_len);
        for _ in 0..pages_len {
            let tag = seq.get_nbt_with_version(&JavaMinecraftVersion::V_26_2)?;
            let comp = tag.as_ref().map_or_else(
                pumpkin_util::text::TextComponent::empty,
                pumpkin_util::text::TextComponent::from_nbt,
            );
            if seq.get_bool()? {
                let _ = seq.get_nbt_with_version(&JavaMinecraftVersion::V_26_2)?;
            }
            pages.push(comp.get_text());
        }
        let _resolved = seq.get_bool()?;
        Ok(Self {
            title,
            author,
            pages,
        })
    }
}

impl DataComponentCodec<Self> for TrimImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt(0))?;
        seq.write_var_int(&VarInt(0))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let _material = seq.get_var_int()?;
        let _pattern = seq.get_var_int()?;
        Ok(Self {
            material: NbtTag::String("minecraft:quartz".into()),
            pattern: NbtTag::String("minecraft:coast".into()),
        })
    }
}

impl DataComponentCodec<Self> for DebugStickStateImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        serialize_nbt_fallback(self, seq)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        deserialize_nbt_fallback(seq, "debug_stick_state", Self::read_data)
    }
}

impl DataComponentCodec<Self> for EntityDataImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt(0))?;
        seq.write_nbt(NbtTag::Compound(pumpkin_nbt::compound::NbtCompound::new()))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let _type_id = seq.get_var_int()?;
        let _nbt = seq.get_nbt_with_version(&JavaMinecraftVersion::V_26_2)?;
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for BucketEntityDataImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_nbt(NbtTag::Compound(pumpkin_nbt::compound::NbtCompound::new()))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let _nbt = seq.get_nbt_with_version(&JavaMinecraftVersion::V_26_2)?;
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for BlockEntityDataImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt(0))?;
        seq.write_nbt(NbtTag::Compound(self.nbt.clone()))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let _type_id = seq.get_var_int()?;
        let tag = seq.get_nbt_with_version(&JavaMinecraftVersion::V_26_2)?;
        let nbt = if let Some(NbtTag::Compound(c)) = tag {
            c
        } else {
            pumpkin_nbt::compound::NbtCompound::new()
        };
        Ok(Self { nbt })
    }
}

impl DataComponentCodec<Self> for InstrumentImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt(0))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let _ = seq.get_var_int()?;
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for ProvidesTrimMaterialImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt(0))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let _ = seq.get_var_int()?;
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for OminousBottleAmplifierImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.amplifier))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let amplifier = seq.get_var_int()?.0;
        Ok(Self { amplifier })
    }
}

impl DataComponentCodec<Self> for JukeboxPlayableImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        let song_name = self.song.strip_prefix("minecraft:").unwrap_or(self.song);
        let song = JukeboxSong::from_name(song_name)
            .ok_or_else(|| WritingError::Message(format!("Unknown jukebox song: {}", self.song)))?;
        // Holder codecs reserve 0 for a direct inline holder; registry IDs are offset by one.
        seq.write_var_int(&VarInt::from(song.get_id() as i32 + 1))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let wire_id = seq.get_var_int()?.0;
        if wire_id == 0 {
            // Direct JukeboxSong holders are legal, but this model only retains registry references.
            return Err(ReadingError::Message(
                "Direct inline jukebox song holders are unsupported".into(),
            ));
        }
        if wire_id < 0 {
            return Err(ReadingError::Message(
                "Negative jukebox song holder ID".into(),
            ));
        }
        let registry_id = (wire_id - 1) as u32;
        let song = JukeboxSong::from_id(registry_id).ok_or_else(|| {
            ReadingError::Message(format!("Unknown jukebox song registry ID: {registry_id}"))
        })?;
        Ok(Self {
            song: song.to_identifier(),
        })
    }
}

impl DataComponentCodec<Self> for ProvidesBannerPatternsImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt(0))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let id_type = seq.get_var_int()?.0;
        if id_type == 0 {
            let _ = seq.get_str()?;
        } else if id_type > 0 {
            for _ in 0..(id_type - 1) {
                let _ = seq.get_var_int()?;
            }
        }
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for RecipesImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        serialize_nbt_fallback(self, seq)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        deserialize_nbt_fallback(seq, "recipes", Self::read_data)
    }
}

impl DataComponentCodec<Self> for LodestoneTrackerImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        if let Some(target) = &self.target {
            seq.write_bool(true)?;
            seq.write_string(&target.dimension)?;
            let pos = pumpkin_util::math::position::BlockPos::new(target.x, target.y, target.z);
            seq.write_block_pos(&pos, &JavaMinecraftVersion::V_26_2)?;
        } else {
            seq.write_bool(false)?;
        }
        seq.write_bool(self.tracked)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let target = if seq.get_bool()? {
            let dimension = seq.get_str()?.to_string();
            let pos = seq.get_block_pos(&JavaMinecraftVersion::V_26_2)?;
            Some(pumpkin_data::data_component_impl::LodestoneTarget {
                dimension,
                x: pos.0.x,
                y: pos.0.y,
                z: pos.0.z,
            })
        } else {
            None
        };
        let tracked = seq.get_bool()?;
        Ok(Self { target, tracked })
    }
}

impl DataComponentCodec<Self> for ProfileImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt(1))?;
        if let Some(name) = &self.name {
            seq.write_bool(true)?;
            seq.write_string(name)?;
        } else {
            seq.write_bool(false)?;
        }
        if let Some(id) = &self.id {
            seq.write_bool(true)?;
            let uuid = uuid::Uuid::from_u128(
                ((id[0] as u128) << 96)
                    | ((id[1] as u128 & 0xFFFFFFFF) << 64)
                    | ((id[2] as u128 & 0xFFFFFFFF) << 32)
                    | (id[3] as u128 & 0xFFFFFFFF),
            );
            seq.write_uuid(&uuid)?;
        } else {
            seq.write_bool(false)?;
        }
        seq.write_var_int(&VarInt::from(self.properties.len() as i32))?;
        for prop in &self.properties {
            seq.write_string(&prop.name)?;
            seq.write_string(&prop.value)?;
            if let Some(sig) = &prop.signature {
                seq.write_bool(true)?;
                seq.write_string(sig)?;
            } else {
                seq.write_bool(false)?;
            }
        }
        if let Some(texture) = &self.texture {
            seq.write_bool(true)?;
            seq.write_string(texture)?;
        } else {
            seq.write_bool(false)?;
        }
        if let Some(cape) = &self.cape {
            seq.write_bool(true)?;
            seq.write_string(cape)?;
        } else {
            seq.write_bool(false)?;
        }
        if let Some(elytra) = &self.elytra {
            seq.write_bool(true)?;
            seq.write_string(elytra)?;
        } else {
            seq.write_bool(false)?;
        }
        if self.model.is_some() {
            seq.write_bool(true)?;
            seq.write_var_int(&VarInt(0))?;
        } else {
            seq.write_bool(false)?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let either = seq.get_var_int()?.0;
        let mut name = None;
        let mut id = None;
        let mut properties = Vec::new();
        if either == 0 {
            let uuid = seq.get_uuid()?;
            let u = uuid.as_u128();
            id = Some([
                (u >> 96) as i32,
                (u >> 64) as i32,
                (u >> 32) as i32,
                u as i32,
            ]);
            name = Some(seq.get_str()?.to_string());
        } else {
            if seq.get_bool()? {
                name = Some(seq.get_str()?.to_string());
            }
            if seq.get_bool()? {
                let uuid = seq.get_uuid()?;
                let u = uuid.as_u128();
                id = Some([
                    (u >> 96) as i32,
                    (u >> 64) as i32,
                    (u >> 32) as i32,
                    u as i32,
                ]);
            }
        }
        let props_len = seq.get_var_int()?.0 as usize;
        for _ in 0..props_len {
            let prop_name = seq.get_str()?.to_string();
            let prop_value = seq.get_str()?.to_string();
            let sig = if seq.get_bool()? {
                Some(seq.get_str()?.to_string())
            } else {
                None
            };
            properties.push(pumpkin_data::data_component_impl::ProfileProperty {
                name: prop_name,
                value: prop_value,
                signature: sig,
            });
        }
        let texture = if seq.get_bool()? {
            Some(seq.get_str()?.to_string())
        } else {
            None
        };
        let cape = if seq.get_bool()? {
            Some(seq.get_str()?.to_string())
        } else {
            None
        };
        let elytra = if seq.get_bool()? {
            Some(seq.get_str()?.to_string())
        } else {
            None
        };
        let model = if seq.get_bool()? {
            let _ = seq.get_var_int()?;
            Some("wide".to_string())
        } else {
            None
        };
        Ok(Self {
            name,
            id,
            properties,
            texture,
            cape,
            elytra,
            model,
        })
    }
}

impl DataComponentCodec<Self> for NoteBlockSoundImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_string(&self.sound)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let sound = seq.get_str()?.to_string();
        Ok(Self { sound })
    }
}

impl DataComponentCodec<Self> for BannerPatternsImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.layers.len() as i32))?;
        for layer in &self.layers {
            seq.write_var_int(&VarInt(0))?;
            seq.write_var_int(&VarInt::from(layer.color.id() as i32))?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let len = seq.get_var_int()?.0 as usize;
        let mut layers = Vec::with_capacity(len);
        for _ in 0..len {
            let _pattern = seq.get_var_int()?.0;
            let color_id = seq.get_var_int()?.0 as u8;
            let color = pumpkin_data::dye_color::DyeColor::by_id(color_id).unwrap_or_default();
            layers.push(pumpkin_data::data_component_impl::BannerPatternLayer {
                pattern: String::new(),
                color,
            });
        }
        Ok(Self { layers })
    }
}

impl DataComponentCodec<Self> for BaseColorImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        let color_id =
            pumpkin_data::dye_color::DyeColor::by_name(&self.color).map_or(0, |c| c.id() as i32);
        seq.write_var_int(&VarInt::from(color_id))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let id = seq.get_var_int()?.0 as u8;
        let color = pumpkin_data::dye_color::DyeColor::by_id(id)
            .map_or("white", |c| c.name())
            .to_string();
        Ok(Self { color })
    }
}

impl DataComponentCodec<Self> for PotDecorationsImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt(0))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let len = seq.get_var_int()?.0 as usize;
        for _ in 0..len {
            let _ = seq.get_var_int()?;
        }
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for ContainerImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        const MAX_CONTAINER_SLOTS: usize = 256;
        let len = self
            .items
            .iter()
            .map(|(slot, _)| usize::from(*slot) + 1)
            .max()
            .unwrap_or(0);
        if len > MAX_CONTAINER_SLOTS {
            return Err(WritingError::Message(
                "Too many Container slots for official limit".into(),
            ));
        }
        seq.write_var_int(&VarInt::from(
            i32::try_from(len)
                .map_err(|_| WritingError::Message("Too many Container slots".into()))?,
        ))?;
        for slot in 0..len {
            if let Some((_, stack)) = self
                .items
                .iter()
                .rev()
                .find(|(item_slot, _)| usize::from(*item_slot) == slot)
            {
                seq.write_bool(true)?;
                serialize_item_stack_template(stack, seq)?;
            } else {
                seq.write_bool(false)?;
            }
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        const MAX_CONTAINER_SLOTS: usize = 256;
        let len = usize::try_from(seq.get_var_int()?.0)
            .map_err(|_| ReadingError::Message("Negative Container count".into()))?;
        if len > MAX_CONTAINER_SLOTS {
            return Err(ReadingError::TooLarge(
                "Too many Container slots for official limit".into(),
            ));
        }
        let mut items = Vec::new();
        for slot in 0..len {
            if seq.get_bool()? {
                items.push((slot as u8, deserialize_item_stack_template(seq)?));
            }
        }
        Ok(Self { items })
    }
}

impl DataComponentCodec<Self> for BlockStateImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt::from(self.properties.len() as i32))?;
        for (k, v) in self.properties.iter() {
            seq.write_string(k)?;
            seq.write_string(v)?;
        }
        Ok(())
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let len = seq.get_var_int()?.0 as usize;
        let mut properties = Vec::with_capacity(len);
        for _ in 0..len {
            let k = seq.get_str()?.to_string();
            let v = seq.get_str()?.to_string();
            properties.push((Cow::Owned(k), Cow::Owned(v)));
        }
        Ok(Self {
            properties: Cow::Owned(properties),
        })
    }
}

impl DataComponentCodec<Self> for BeesImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt(0))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let len = seq.get_var_int()?.0 as usize;
        for _ in 0..len {
            let _entity_type = seq.get_var_int()?;
            let _nbt = seq.get_nbt_with_version(&JavaMinecraftVersion::V_26_2)?;
            let _ticks = seq.get_var_int()?;
            let _min_ticks = seq.get_var_int()?;
        }
        Ok(Self)
    }
}

impl DataComponentCodec<Self> for SulfurCubeContentImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        serialize_item_stack_template(&self.absorbed_block_item_stack, seq)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        Ok(Self {
            absorbed_block_item_stack: deserialize_item_stack_template(seq)?,
        })
    }
}

impl DataComponentCodec<Self> for LockImpl {
    fn serialize(&self, _seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        Ok(())
    }

    fn deserialize(_seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        Ok(Self {
            predicate: pumpkin_nbt::compound::NbtCompound::new(),
        })
    }
}

impl DataComponentCodec<Self> for ContainerLootImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        serialize_nbt_fallback(self, seq)
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        deserialize_nbt_fallback(seq, "container_loot", Self::read_data)
    }
}

impl DataComponentCodec<Self> for BreakSoundImpl {
    fn serialize(&self, seq: &mut impl NetworkWriteExt) -> Result<(), WritingError> {
        seq.write_var_int(&VarInt(0))
    }

    fn deserialize(seq: &mut impl NetworkReadExt) -> Result<Self, ReadingError> {
        let _ = seq.get_var_int()?;
        Ok(Self)
    }
}

#[cfg(test)]
mod jukebox_playable_tests {
    use super::*;

    #[test]
    fn recipes_codec_consumes_official_nbt_wire_and_preserves_following_component() {
        // DataComponentType.Builder.build() falls back to ByteBufCodecs.fromCodecWithRegistries:
        // FriendlyByteBuf.writeNbt(List<String>) => 0x09, element type 0x08, big-endian count,
        // then UTF-8 strings. The trailing 64 is the next MaxStackSize component value.
        let mut wire = vec![9, 8, 0, 0, 0, 2, 0, 33];
        wire.extend_from_slice(b"minecraft:iron_ingot_from_nuggets");
        wire.extend_from_slice(&[0, 21]);
        wire.extend_from_slice(b"example:custom_recipe");
        wire.push(64);

        let mut input = wire.as_slice();
        let decoded = RecipesImpl::deserialize(&mut input).unwrap();
        assert_eq!(
            decoded.recipes,
            vec![
                "minecraft:iron_ingot_from_nuggets".to_owned(),
                "example:custom_recipe".to_owned(),
            ]
        );
        assert_eq!(MaxStackSizeImpl::deserialize(&mut input).unwrap().size, 64);
        assert!(input.is_empty());

        let mut encoded = Vec::new();
        decoded.serialize(&mut encoded).unwrap();
        assert_eq!(encoded, &wire[..wire.len() - 1]);
    }

    #[test]
    fn jukebox_playable_wire_round_trip_covers_all_registry_indices() {
        for registry_id in 0..=21u32 {
            let song = JukeboxSong::from_id(registry_id).unwrap();
            assert_eq!(song.get_id(), registry_id);
            assert_eq!(
                song.to_identifier(),
                format!("minecraft:{}", song.to_name())
            );

            let wire = [(registry_id + 1) as u8];
            let mut input = wire.as_slice();
            let decoded = JukeboxPlayableImpl::deserialize(&mut input).unwrap();
            assert!(input.is_empty());
            assert_eq!(decoded.song, song.to_identifier());

            let mut encoded = Vec::new();
            decoded.serialize(&mut encoded).unwrap();
            assert_eq!(encoded, wire);
        }
    }

    #[test]
    fn jukebox_playable_wire_rejects_direct_and_unknown_holders() {
        let mut direct = [0].as_slice();
        assert!(JukeboxPlayableImpl::deserialize(&mut direct).is_err());

        let mut unknown = [0x17].as_slice();
        assert!(JukeboxPlayableImpl::deserialize(&mut unknown).is_err());
    }
}

#[cfg(test)]
mod persistent_codec_fallback_tests {
    use super::*;

    #[test]
    fn item_name_stream_codec_preserves_component_identity_and_style() {
        let literal = ItemNameImpl {
            name: pumpkin_data::data_component_impl::ItemName::Component(
                pumpkin_util::text::TextComponent::text("item.minecraft.apple"),
            ),
        };
        let translated = ItemNameImpl {
            name: pumpkin_data::data_component_impl::ItemName::Component(
                pumpkin_util::text::TextComponent::translate("item.minecraft.apple", vec![])
                    .bold(),
            ),
        };
        assert_ne!(literal, translated);

        for original in [literal, translated] {
            let mut wire = Vec::new();
            original.serialize(&mut wire).expect("serialize ItemName");
            let decoded = ItemNameImpl::deserialize(&mut wire.as_slice()).expect("decode ItemName");
            assert_eq!(decoded, original);
        }

        let mut malformed = vec![pumpkin_nbt::INT_ID];
        malformed.extend_from_slice(&1_i32.to_be_bytes());
        assert!(ItemNameImpl::deserialize(&mut malformed.as_slice()).is_err());
    }

    #[test]
    fn item_name_stream_codec_accepts_non_empty_lists_and_rejects_empty_lists() {
        for (tag, is_valid) in [
            (
                NbtTag::List(vec![NbtTag::String("first".into())]),
                true,
            ),
            (NbtTag::List(vec![]), false),
        ] {
            let mut wire = Vec::new();
            tag.serialize(&mut NbtWriteHelperJava::new(&mut wire))
                .expect("serialize ItemName NBT");
            assert_eq!(
                ItemNameImpl::deserialize(&mut wire.as_slice()).is_ok(),
                is_valid
            );
        }
    }

    #[test]
    fn debug_stick_state_uses_official_nbt_fallback_and_preserves_next_component() {
        let mut wire = vec![0x0a, 0x08, 0x00, 0x11];
        wire.extend_from_slice(b"minecraft:oak_log");
        wire.extend_from_slice(&[0x00, 0x04]);
        wire.extend_from_slice(b"axis");
        wire.extend_from_slice(&[0x00, 0x40]);

        let mut input = wire.as_slice();
        let decoded = deserialize(DataComponent::DebugStickState, &mut input)
            .expect("debug stick state fallback should decode");
        let decoded = decoded
            .as_any()
            .downcast_ref::<DebugStickStateImpl>()
            .expect("debug stick state implementation");
        let mut expected = NbtCompound::new();
        expected.put_string("minecraft:oak_log", "axis".to_owned());
        assert_eq!(decoded.write_data(), NbtTag::Compound(expected));
        assert_eq!(MaxStackSizeImpl::deserialize(&mut input).unwrap().size, 64);
        assert!(input.is_empty());

        let mut empty = [0x0a, 0x00, 0x40].as_slice();
        let decoded = deserialize(DataComponent::DebugStickState, &mut empty)
            .expect("empty debug stick state should decode");
        assert_eq!(decoded.write_data(), NbtTag::Compound(NbtCompound::new()));
        assert_eq!(MaxStackSizeImpl::deserialize(&mut empty).unwrap().size, 64);
        assert!(empty.is_empty());
    }

    #[test]
    fn debug_stick_state_rejects_malformed_and_invalid_entries() {
        let mut end = [0x00].as_slice();
        assert!(deserialize(DataComponent::DebugStickState, &mut end).is_err());

        for (block, property) in [
            ("minecraft:no_such_block", "axis"),
            ("minecraft:oak_log", "no_such_property"),
            ("custom:oak_log", "axis"),
        ] {
            let mut root = NbtCompound::new();
            root.put_string(block, property.to_owned());
            let mut wire = Vec::new();
            wire.write_nbt(NbtTag::Compound(root)).unwrap();
            assert!(deserialize(DataComponent::DebugStickState, &mut wire.as_slice()).is_err());
        }

        let mut root = NbtCompound::new();
        root.put_int("minecraft:stone", 1);
        let mut wire = Vec::new();
        wire.write_nbt(NbtTag::Compound(root)).unwrap();
        assert!(deserialize(DataComponent::DebugStickState, &mut wire.as_slice()).is_err());
    }

    #[test]
    fn debug_stick_state_hash_and_equality_follow_canonical_map_state() {
        let mut first = std::collections::BTreeMap::new();
        first.insert("minecraft:oak_log".to_owned(), "axis".to_owned());
        first.insert("minecraft:oak_fence_gate".to_owned(), "facing".to_owned());

        let mut second = std::collections::BTreeMap::new();
        second.insert("minecraft:oak_fence_gate".to_owned(), "facing".to_owned());
        second.insert("minecraft:oak_log".to_owned(), "axis".to_owned());

        let left = DebugStickStateImpl { properties: first };
        let right = DebugStickStateImpl { properties: second };
        assert_eq!(left, right);
        assert_eq!(left.get_hash(), right.get_hash());

        let mut changed = right.clone();
        changed
            .properties
            .insert("minecraft:oak_log".to_owned(), "waterlogged".to_owned());
        assert_ne!(left, changed);
        assert_ne!(left.get_hash(), changed.get_hash());
    }

    #[test]
    fn debug_stick_state_adventure_exact_round_trip_uses_same_fallback() {
        let mut state = NbtCompound::new();
        state.put_string("minecraft:oak_log", "axis".to_owned());
        let mut components = NbtCompound::new();
        components.put("minecraft:debug_stick_state", NbtTag::Compound(state));
        let mut root = NbtCompound::new();
        root.put("components", NbtTag::Compound(components));
        let value = NbtTag::Compound(root);

        let mut wire = Vec::new();
        write_adventure_components(&value, &mut wire).unwrap();
        let decoded = read_adventure_components(&mut wire.as_slice()).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn intangible_projectile_accepts_unknown_compound_fields_and_preserves_next_component() {
        let expected = [
            0x0a, 0x08, 0x00, 0x07, b'u', b'n', b'k', b'n', b'o', b'w', b'n', 0x00, 0x07, b'p',
            b'a', b'y', b'l', b'o', b'a', b'd', 0x00, 0x40,
        ];
        let mut input = expected.as_slice();
        let decoded = deserialize(DataComponent::IntangibleProjectile, &mut input)
            .expect("intangible projectile fallback should decode");
        assert!(
            decoded
                .as_any()
                .downcast_ref::<IntangibleProjectileImpl>()
                .is_some()
        );
        assert_eq!(
            MaxStackSizeImpl::deserialize(&mut input)
                .expect("following component should remain aligned")
                .size,
            64
        );
        assert!(input.is_empty());

        let mut encoded = Vec::new();
        serialize(
            DataComponent::IntangibleProjectile,
            decoded.as_ref(),
            &mut encoded,
        )
        .expect("intangible projectile fallback should encode");
        assert_eq!(encoded, [0x0a, 0x00]);
    }

    #[test]
    fn container_loot_uses_official_nbt_fallback_and_preserves_next_component() {
        let mut expected = vec![0x0a, 0x08, 0x00, 0x0a];
        expected.extend_from_slice(b"loot_table");
        expected.extend_from_slice(&[0x00, 0x1f]);
        expected.extend_from_slice(b"minecraft:chests/simple_dungeon");
        expected.extend_from_slice(&[0x04, 0x00, 0x04]);
        expected.extend_from_slice(b"seed");
        expected.extend_from_slice(&123_456_789i64.to_be_bytes());
        expected.extend_from_slice(&[0x00, 0x40]);

        let mut input = expected.as_slice();
        let decoded = deserialize(DataComponent::ContainerLoot, &mut input)
            .expect("container loot fallback should decode");
        let decoded = decoded
            .as_any()
            .downcast_ref::<ContainerLootImpl>()
            .expect("container loot implementation");
        assert_eq!(decoded.loot_table, "minecraft:chests/simple_dungeon");
        assert_eq!(decoded.seed, 123_456_789);
        assert_eq!(
            MaxStackSizeImpl::deserialize(&mut input)
                .expect("following component should remain aligned")
                .size,
            64
        );
        assert!(input.is_empty());

        let mut encoded = Vec::new();
        serialize(DataComponent::ContainerLoot, decoded, &mut encoded)
            .expect("container loot fallback should encode");
        let round_trip = deserialize(DataComponent::ContainerLoot, &mut encoded.as_slice())
            .expect("encoded container loot should decode");
        let round_trip = round_trip
            .as_any()
            .downcast_ref::<ContainerLootImpl>()
            .unwrap();
        assert_eq!(round_trip.loot_table, decoded.loot_table);
        assert_eq!(round_trip.seed, decoded.seed);
    }

    #[test]
    fn container_loot_omits_official_default_seed() {
        let mut expected = vec![0x0a, 0x08, 0x00, 0x0a];
        expected.extend_from_slice(b"loot_table");
        expected.extend_from_slice(&[0x00, 0x1f]);
        expected.extend_from_slice(b"minecraft:chests/simple_dungeon");
        expected.push(0x00);

        let value = ContainerLootImpl {
            loot_table: "minecraft:chests/simple_dungeon".to_owned(),
            seed: 0,
        };
        let mut encoded = Vec::new();
        serialize(DataComponent::ContainerLoot, &value, &mut encoded)
            .expect("default container loot should encode");
        assert_eq!(encoded, expected);

        let mut input = encoded.as_slice();
        let decoded = deserialize(DataComponent::ContainerLoot, &mut input)
            .expect("default container loot should decode");
        let decoded = decoded
            .as_any()
            .downcast_ref::<ContainerLootImpl>()
            .unwrap();
        assert_eq!(decoded.seed, 0);
        assert!(input.is_empty());
    }

    #[test]
    fn map_decorations_fallback_preserves_entries_and_next_component() {
        let mut player = NbtCompound::new();
        player.put_string("type", "minecraft:player".to_owned());
        player.put_double("x", 1234.5);
        player.put_double("z", -987.25);
        player.put_float("rotation", 1.5);

        let mut custom = NbtCompound::new();
        custom.put_string("type", "custom:marker".to_owned());
        custom.put_double("x", -0.25);
        custom.put_double("z", 4.0);
        custom.put_float("rotation", -2.25);

        let mut root = NbtCompound::new();
        root.put_compound("player", player);
        root.put_compound("custom marker", custom);

        let expected = NbtTag::Compound(root);
        let mut wire = Vec::new();
        wire.write_nbt(expected.clone()).unwrap();
        wire.push(64); // following MaxStackSize component

        let mut input = wire.as_slice();
        let decoded = deserialize(DataComponent::MapDecorations, &mut input)
            .expect("map decorations fallback should decode");
        assert_eq!(decoded.write_data(), expected);
        assert_eq!(
            MaxStackSizeImpl::deserialize(&mut input)
                .expect("following component should remain aligned")
                .size,
            64
        );
        assert!(input.is_empty());
    }

    #[test]
    fn map_decorations_accepts_numeric_tags_and_ignores_extra_fields() {
        let mut entry = NbtCompound::new();
        entry.put_string("type", "player".to_owned());
        entry.put_byte("x", -7);
        entry.put_long("z", 123);
        entry.put_double("rotation", 1.75);
        entry.put("future_field", NbtTag::List(Vec::new()));

        let mut root = NbtCompound::new();
        root.put_compound("future marker", entry);

        let mut wire = Vec::new();
        wire.write_nbt(NbtTag::Compound(root)).unwrap();
        let decoded = deserialize(DataComponent::MapDecorations, &mut wire.as_slice())
            .expect("numeric map decoration fields should decode");
        let encoded = decoded.write_data();
        let entry = encoded
            .extract_compound()
            .and_then(|root| root.get_compound("future marker"))
            .expect("decoded map decoration entry");
        assert_eq!(entry.get_string("type"), Some("minecraft:player"));
        assert_eq!(entry.get("x"), Some(&NbtTag::Double(-7.0)));
        assert_eq!(entry.get("z"), Some(&NbtTag::Double(123.0)));
        assert_eq!(entry.get("rotation"), Some(&NbtTag::Float(1.75)));
        assert!(!entry.has("future_field"));
    }

    #[test]
    fn map_decorations_rejects_invalid_shape_and_identifier() {
        let mut end = [0].as_slice();
        assert!(deserialize(DataComponent::MapDecorations, &mut end).is_err());
        let mut list = Vec::new();
        list.write_nbt(NbtTag::List(Vec::new())).unwrap();
        assert!(deserialize(DataComponent::MapDecorations, &mut list.as_slice()).is_err());

        let mut root = NbtCompound::new();
        root.put_string("marker", "not an entry".to_owned());
        let mut wrong_entry = Vec::new();
        wrong_entry.write_nbt(NbtTag::Compound(root)).unwrap();
        assert!(deserialize(DataComponent::MapDecorations, &mut wrong_entry.as_slice()).is_err());

        let mut entry = NbtCompound::new();
        entry.put_string("type", "bad:id:extra".to_owned());
        entry.put_double("x", 0.0);
        entry.put_double("z", 0.0);
        entry.put_float("rotation", 0.0);
        let mut root = NbtCompound::new();
        root.put_compound("marker", entry);
        let mut invalid_id = Vec::new();
        invalid_id.write_nbt(NbtTag::Compound(root)).unwrap();
        assert!(deserialize(DataComponent::MapDecorations, &mut invalid_id.as_slice()).is_err());
    }

    #[test]
    fn map_decorations_adventure_exact_round_trip_uses_same_fallback() {
        let mut entry = NbtCompound::new();
        entry.put_string("type", "minecraft:player".to_owned());
        entry.put_double("x", 1234.5);
        entry.put_double("z", -987.25);
        entry.put_float("rotation", 1.5);
        let mut decorations = NbtCompound::new();
        decorations.put_compound("player", entry);

        let mut component = NbtCompound::new();
        component.put(
            "minecraft:map_decorations",
            NbtTag::Compound(decorations.clone()),
        );
        let value = NbtTag::Compound({
            let mut value = NbtCompound::new();
            value.put_compound("components", component);
            value
        });

        let mut wire = Vec::new();
        write_adventure_components(&value, &mut wire).unwrap();
        let expected_prefix = [1, DataComponent::MapDecorations.to_id()];
        assert_eq!(&wire[..expected_prefix.len()], expected_prefix);
        assert_eq!(*wire.last().unwrap(), 0);

        let decoded = read_adventure_components(&mut wire.as_slice()).unwrap();
        assert_eq!(decoded, value);
        let mut encoded = Vec::new();
        write_adventure_components(&decoded, &mut encoded).unwrap();
        let redecoded = read_adventure_components(&mut encoded.as_slice()).unwrap();
        assert_eq!(redecoded, value);
    }

    #[test]
    fn persistent_fallback_rejects_nbt_end_and_wrong_types() {
        let mut end = [0x00].as_slice();
        assert!(deserialize(DataComponent::IntangibleProjectile, &mut end).is_err());

        let mut wrong_type = [0x08, 0x00, 0x40].as_slice();
        assert!(deserialize(DataComponent::IntangibleProjectile, &mut wrong_type).is_err());

        let mut primitive_root = [0x03, 0x00, 0x00, 0x00, 0x07, 0x40].as_slice();
        assert!(deserialize(DataComponent::IntangibleProjectile, &mut primitive_root).is_err());

        let mut invalid_loot = [0x08, 0x00, 0x40].as_slice();
        assert!(deserialize(DataComponent::ContainerLoot, &mut invalid_loot).is_err());

        let mut missing_loot_table = [0x0a, 0x00, 0x40].as_slice();
        assert!(deserialize(DataComponent::ContainerLoot, &mut missing_loot_table).is_err());

        let mut invalid_seed = Vec::new();
        let mut compound = NbtCompound::new();
        compound.put_string("loot_table", "minecraft:chests/simple_dungeon".to_owned());
        compound.put_string("seed", "not a long".to_owned());
        invalid_seed.write_nbt(NbtTag::Compound(compound)).unwrap();
        invalid_seed.push(64);
        assert!(deserialize(DataComponent::ContainerLoot, &mut invalid_seed.as_slice()).is_err());
    }

    #[test]
    fn container_loot_accepts_all_nbt_numeric_seed_tags_like_codec_long() {
        let cases = vec![
            (NbtTag::Byte(-7), -7),
            (NbtTag::Short(1234), 1234),
            (NbtTag::Int(-56789), -56789),
            (NbtTag::Long(i64::MAX), i64::MAX),
            (NbtTag::Float(1.75), 1),
            (NbtTag::Double(-1.75), -1),
            (NbtTag::Float(f32::INFINITY), i64::MAX),
            (NbtTag::Double(f64::NEG_INFINITY), i64::MIN),
            (NbtTag::Double(f64::NAN), 0),
        ];

        for (seed, expected_seed) in cases {
            let mut compound = NbtCompound::new();
            compound.put_string("loot_table", "custom/loot".to_owned());
            compound.put("seed", seed);
            let mut wire = Vec::new();
            wire.write_nbt(NbtTag::Compound(compound)).unwrap();

            let decoded = deserialize(DataComponent::ContainerLoot, &mut wire.as_slice())
                .expect("numeric seed should match Codec.LONG conversion");
            let decoded = decoded
                .as_any()
                .downcast_ref::<ContainerLootImpl>()
                .unwrap();
            assert_eq!(decoded.loot_table, "minecraft:custom/loot");
            assert_eq!(decoded.seed, expected_seed);
        }
    }

    #[test]
    fn adventure_exact_match_uses_persistent_fallback_codec() {
        let mut wire = Vec::new();
        wire.write_var_int(&VarInt(1)).unwrap();
        wire.write_var_int(&VarInt(DataComponent::IntangibleProjectile.to_id() as i32))
            .unwrap();
        wire.extend_from_slice(&[0x0a, 0x00]);
        wire.write_var_int(&VarInt(0)).unwrap();

        let mut input = wire.as_slice();
        let decoded = read_adventure_components(&mut input).unwrap();
        assert!(input.is_empty());
        let components = decoded
            .extract_compound()
            .unwrap()
            .get_compound("components")
            .unwrap();
        assert_eq!(
            components.get_compound("minecraft:intangible_projectile"),
            Some(&NbtCompound::new())
        );

        let mut encoded = Vec::new();
        write_adventure_components(&decoded, &mut encoded).unwrap();
        assert_eq!(encoded, wire);
    }
}

#[cfg(test)]
mod death_protection_tests {
    use super::*;

    #[test]
    fn death_protection_wire_and_nbt_round_trip_preserves_effects() {
        let wire = [0x01, 0x02];
        let mut input = wire.as_slice();
        let decoded = DeathProtectionImpl::deserialize(&mut input).unwrap();
        assert!(input.is_empty());

        let mut encoded = Vec::new();
        decoded.serialize(&mut encoded).unwrap();
        assert_eq!(encoded, wire);

        let mut clear = NbtCompound::new();
        clear.put_string("type", "minecraft:clear_all_effects".to_owned());
        let mut status = NbtCompound::new();
        status.put_string("id", "minecraft:regeneration".to_owned());
        status.put_int("amplifier", 1);
        status.put_int("duration", 900);
        status.put_bool("ambient", false);
        status.put_bool("show_particles", true);
        status.put_bool("show_icon", true);
        let mut apply = NbtCompound::new();
        apply.put_string("type", "minecraft:apply_effects".to_owned());
        apply.put_float("probability", 1.0);
        apply.put_list("effects", vec![NbtTag::Compound(status)]);
        let mut nbt = NbtCompound::new();
        nbt.put_list(
            "death_effects",
            vec![NbtTag::Compound(clear), NbtTag::Compound(apply)],
        );
        let value = DeathProtectionImpl::read_data(&NbtTag::Compound(nbt.clone())).unwrap();
        assert_eq!(value.death_effects.len(), 2);
        assert_eq!(value.write_data(), NbtTag::Compound(nbt));
    }

    #[test]
    fn death_protection_rejects_invalid_effect_counts_and_tags() {
        let mut invalid_effect = NbtCompound::new();
        invalid_effect.put_string("type", "minecraft:not_a_consume_effect".to_owned());
        let mut invalid_nbt = NbtCompound::new();
        invalid_nbt.put_list("death_effects", vec![NbtTag::Compound(invalid_effect)]);
        assert!(DeathProtectionImpl::read_data(&NbtTag::Compound(invalid_nbt)).is_none());

        let mut wrong_type = NbtCompound::new();
        wrong_type.put_string("death_effects", "not a list".to_owned());
        assert!(DeathProtectionImpl::read_data(&NbtTag::Compound(wrong_type)).is_none());

        let mut negative = Vec::new();
        negative.write_var_int(&VarInt(-1)).unwrap();
        assert!(DeathProtectionImpl::deserialize(&mut negative.as_slice()).is_err());

        let mut too_many = Vec::new();
        too_many.write_var_int(&VarInt(257)).unwrap();
        assert!(DeathProtectionImpl::deserialize(&mut too_many.as_slice()).is_err());

        let mut huge_idset = Vec::new();
        huge_idset.write_var_int(&VarInt(1)).unwrap();
        huge_idset.write_var_int(&VarInt(1)).unwrap();
        huge_idset.write_var_int(&VarInt(258)).unwrap();
        assert!(DeathProtectionImpl::deserialize(&mut huge_idset.as_slice()).is_err());

        let mut unknown_remove = NbtCompound::new();
        unknown_remove.put_string("type", "minecraft:remove_effects".to_owned());
        unknown_remove.put_list(
            "effects",
            vec![NbtTag::String("minecraft:not_a_status_effect".into())],
        );
        let mut unknown_remove_nbt = NbtCompound::new();
        unknown_remove_nbt.put_list("death_effects", vec![NbtTag::Compound(unknown_remove)]);
        assert!(DeathProtectionImpl::read_data(&NbtTag::Compound(unknown_remove_nbt)).is_none());

        let mut unknown_sound = NbtCompound::new();
        unknown_sound.put_string("type", "minecraft:play_sound".to_owned());
        unknown_sound.put_string("sound", "minecraft:not_a_sound".to_owned());
        let mut unknown_sound_nbt = NbtCompound::new();
        unknown_sound_nbt.put_list("death_effects", vec![NbtTag::Compound(unknown_sound)]);
        assert!(DeathProtectionImpl::read_data(&NbtTag::Compound(unknown_sound_nbt)).is_none());

        let mut unknown_status = NbtCompound::new();
        unknown_status.put_string("id", "minecraft:not_a_status_effect".to_owned());
        unknown_status.put_int("amplifier", 0);
        unknown_status.put_int("duration", 1);
        unknown_status.put_bool("ambient", false);
        unknown_status.put_bool("show_particles", true);
        unknown_status.put_bool("show_icon", true);
        let mut unknown_apply = NbtCompound::new();
        unknown_apply.put_string("type", "minecraft:apply_effects".to_owned());
        unknown_apply.put_float("probability", 1.0);
        unknown_apply.put_list("effects", vec![NbtTag::Compound(unknown_status)]);
        let mut unknown_status_nbt = NbtCompound::new();
        unknown_status_nbt.put_list("death_effects", vec![NbtTag::Compound(unknown_apply)]);
        assert!(DeathProtectionImpl::read_data(&NbtTag::Compound(unknown_status_nbt)).is_none());
    }
}

#[cfg(test)]
mod tooltip_display_tests {
    use super::*;

    #[test]
    fn tooltip_display_wire_round_trip_preserves_values_and_rejects_invalid_inputs() {
        let mut wire = Vec::new();
        wire.write_bool(true).unwrap();
        wire.write_var_int(&VarInt(2)).unwrap();
        wire.write_var_int(&VarInt(DataComponent::TooltipStyle.to_id() as i32))
            .unwrap();
        wire.write_var_int(&VarInt(DataComponent::Lore.to_id() as i32))
            .unwrap();

        let mut input = wire.as_slice();
        let decoded = deserialize(DataComponent::TooltipDisplay, &mut input).unwrap();
        assert!(input.is_empty());

        let mut expected = NbtCompound::new();
        expected.put_bool("hide_tooltip", true);
        expected.put_list(
            "hidden_components",
            vec![
                NbtTag::String(DataComponent::TooltipStyle.to_name().into()),
                NbtTag::String(DataComponent::Lore.to_name().into()),
            ],
        );
        let nbt_decoded = TooltipDisplayImpl::read_data(&NbtTag::Compound(expected.clone()))
            .expect("TooltipDisplay NBT should decode");
        assert!(nbt_decoded.hide_tooltip);
        assert!(
            nbt_decoded.hidden_components == vec![DataComponent::TooltipStyle, DataComponent::Lore]
        );
        assert_eq!(decoded.write_data(), NbtTag::Compound(expected.clone()));

        let mut invalid_nbt = expected;
        invalid_nbt.put_list(
            "hidden_components",
            vec![NbtTag::String("minecraft:not_a_component".into())],
        );
        assert!(TooltipDisplayImpl::read_data(&NbtTag::Compound(invalid_nbt)).is_none());

        let mut encoded = Vec::new();
        serialize(
            DataComponent::TooltipDisplay,
            decoded.as_ref(),
            &mut encoded,
        )
        .unwrap();
        assert_eq!(encoded, wire);

        let mut unknown_id = Vec::new();
        unknown_id.write_bool(false).unwrap();
        unknown_id.write_var_int(&VarInt(1)).unwrap();
        unknown_id.write_var_int(&VarInt(255)).unwrap();
        assert!(deserialize(DataComponent::TooltipDisplay, &mut unknown_id.as_slice()).is_err());

        let mut negative_count = Vec::new();
        negative_count.write_bool(false).unwrap();
        negative_count.write_var_int(&VarInt(-1)).unwrap();
        assert!(
            deserialize(
                DataComponent::TooltipDisplay,
                &mut negative_count.as_slice()
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod attribute_modifier_tests {
    use super::*;

    fn write_modifier(
        wire: &mut Vec<u8>,
        attribute: i32,
        id: &str,
        amount: f64,
        operation: i32,
        slot: i32,
        display: i32,
    ) {
        wire.write_var_int(&VarInt(attribute)).unwrap();
        wire.write_string(id).unwrap();
        wire.write_f64(amount).unwrap();
        wire.write_var_int(&VarInt(operation)).unwrap();
        wire.write_var_int(&VarInt(slot)).unwrap();
        wire.write_var_int(&VarInt(display)).unwrap();
    }

    #[test]
    fn independent_wire_round_trip_preserves_all_modifier_fields() {
        let mut wire = Vec::new();
        wire.write_var_int(&VarInt(3)).unwrap();
        write_modifier(&mut wire, 3, "minecraft:base_attack_damage", -2.5, 0, 1, 0);
        write_modifier(
            &mut wire,
            26,
            "custom:non_finite_nan",
            f64::from_bits(0x7ff8_0000_0000_0042),
            2,
            10,
            1,
        );
        write_modifier(
            &mut wire,
            23,
            "minecraft:health_bonus",
            f64::INFINITY,
            1,
            8,
            2,
        );
        let mut display = NbtCompound::new();
        display.put_string("text", "override".to_owned());
        wire.write_nbt(NbtTag::Compound(display)).unwrap();
        let mut input = wire.as_slice();
        let decoded = AttributeModifiersImpl::deserialize(&mut input).unwrap();
        assert!(input.is_empty());
        assert_eq!(decoded.attribute_modifiers.len(), 3);
        assert_eq!(decoded.attribute_modifiers[0].r#type.id, 3);
        assert_eq!(
            decoded.attribute_modifiers[0].id.as_ref(),
            "minecraft:base_attack_damage"
        );
        assert_eq!(decoded.attribute_modifiers[0].amount, -2.5);
        assert_eq!(
            decoded.attribute_modifiers[0].operation,
            Operation::AddValue
        );
        assert_eq!(
            decoded.attribute_modifiers[0].slot,
            pumpkin_data::enchantment::AttributeModifierSlot::MainHand
        );
        assert!(matches!(
            decoded.attribute_modifiers[0].display,
            ModifierDisplay::Default
        ));
        assert_eq!(
            decoded.attribute_modifiers[1].r#type.name,
            "minecraft:movement_speed"
        );
        assert_eq!(
            decoded.attribute_modifiers[1].amount.to_bits(),
            0x7ff8_0000_0000_0042
        );
        assert_eq!(
            decoded.attribute_modifiers[1].operation,
            Operation::AddMultipliedTotal
        );
        assert_eq!(
            decoded.attribute_modifiers[1].slot,
            pumpkin_data::enchantment::AttributeModifierSlot::Saddle
        );
        assert!(matches!(
            decoded.attribute_modifiers[1].display,
            ModifierDisplay::Hidden
        ));
        assert!(decoded.attribute_modifiers[2].amount.is_infinite());
        assert!(matches!(
            decoded.attribute_modifiers[2].display,
            ModifierDisplay::Override(_)
        ));

        let mut encoded = Vec::new();
        decoded.serialize(&mut encoded).unwrap();
        assert_eq!(encoded, wire);
    }

    #[test]
    fn attribute_modifier_rejects_invalid_counts_ids_and_enums() {
        let mut negative = &[0xff, 0xff, 0xff, 0xff, 0x0f][..];
        assert!(AttributeModifiersImpl::deserialize(&mut negative).is_err());

        let unknown_attribute = vec![1, 40];
        assert!(AttributeModifiersImpl::deserialize(&mut unknown_attribute.as_slice()).is_err());

        let mut unknown_operation = Vec::new();
        unknown_operation.write_var_int(&VarInt(1)).unwrap();
        write_modifier(&mut unknown_operation, 0, "minecraft:test", 0.0, 3, 0, 0);
        assert!(AttributeModifiersImpl::deserialize(&mut unknown_operation.as_slice()).is_err());

        let mut unknown_slot = Vec::new();
        unknown_slot.write_var_int(&VarInt(1)).unwrap();
        write_modifier(&mut unknown_slot, 0, "minecraft:test", 0.0, 0, 11, 0);
        assert!(AttributeModifiersImpl::deserialize(&mut unknown_slot.as_slice()).is_err());

        let mut unknown_display = Vec::new();
        unknown_display.write_var_int(&VarInt(1)).unwrap();
        write_modifier(&mut unknown_display, 0, "minecraft:test", 0.0, 0, 0, 3);
        assert!(AttributeModifiersImpl::deserialize(&mut unknown_display.as_slice()).is_err());

        let mut invalid_id = Vec::new();
        invalid_id.write_var_int(&VarInt(1)).unwrap();
        write_modifier(
            &mut invalid_id,
            0,
            "minecraft:not a resource id",
            0.0,
            0,
            0,
            0,
        );
        assert!(AttributeModifiersImpl::deserialize(&mut invalid_id.as_slice()).is_err());
    }
}

#[cfg(test)]
mod adventure_predicate_tests {
    use super::*;

    #[test]
    fn can_place_on_wire_round_trip_preserves_predicate_fields() {
        let mut wire = Vec::new();
        wire.write_var_int(&VarInt(1)).unwrap();
        wire.write_bool(true).unwrap();
        let stone = Block::from_name("minecraft:stone").unwrap();
        wire.write_var_int(&VarInt(2)).unwrap();
        wire.write_var_int(&VarInt(i32::from(stone.registry_id())))
            .unwrap();
        wire.write_bool(true).unwrap();
        wire.write_var_int(&VarInt(2)).unwrap();
        wire.write_string("facing").unwrap();
        wire.write_bool(true).unwrap();
        wire.write_string("north").unwrap();
        wire.write_string("age").unwrap();
        wire.write_bool(false).unwrap();
        wire.write_bool(true).unwrap();
        wire.write_string("1").unwrap();
        wire.write_bool(true).unwrap();
        wire.write_string("3").unwrap();
        wire.write_bool(true).unwrap();
        let mut nbt = NbtCompound::new();
        nbt.put_string("id", "minecraft:chest".to_string());
        wire.write_nbt(NbtTag::Compound(nbt)).unwrap();
        wire.write_var_int(&VarInt(3)).unwrap();
        wire.write_var_int(&VarInt(DataComponent::CustomData.to_id() as i32))
            .unwrap();
        let mut custom = NbtCompound::new();
        custom.put_string("marker", "preserved".to_string());
        wire.write_nbt(NbtTag::Compound(custom)).unwrap();
        wire.write_var_int(&VarInt(DataComponent::AttributeModifiers.to_id() as i32))
            .unwrap();
        wire.write_var_int(&VarInt(1)).unwrap();
        wire.write_var_int(&VarInt(3)).unwrap();
        wire.write_string("minecraft:nested_exact").unwrap();
        wire.write_f64(1.25).unwrap();
        wire.write_var_int(&VarInt(2)).unwrap();
        wire.write_var_int(&VarInt(3)).unwrap();
        wire.write_var_int(&VarInt(1)).unwrap();
        wire.write_var_int(&VarInt(DataComponent::TooltipDisplay.to_id() as i32))
            .unwrap();
        wire.write_bool(true).unwrap();
        wire.write_var_int(&VarInt(1)).unwrap();
        wire.write_var_int(&VarInt(DataComponent::TooltipStyle.to_id() as i32))
            .unwrap();
        wire.write_var_int(&VarInt(1)).unwrap();
        wire.write_var_int(&VarInt(DataComponent::TooltipDisplay.to_id() as i32))
            .unwrap();

        let mut input = wire.as_slice();
        let decoded = CanPlaceOnImpl::deserialize(&mut input).unwrap();
        assert!(input.is_empty());
        let predicate = decoded.predicate.extract_list().unwrap()[0]
            .extract_compound()
            .unwrap();
        let components = predicate
            .get_compound("components")
            .unwrap()
            .get_compound("components")
            .unwrap();
        let modifiers = components
            .get_list("minecraft:attribute_modifiers")
            .unwrap();
        assert_eq!(modifiers.len(), 1);
        assert_eq!(
            modifiers[0].extract_compound().unwrap().get_string("type"),
            Some("minecraft:attack_damage")
        );
        let mut encoded = Vec::new();
        decoded.serialize(&mut encoded).unwrap();
        let mut redecoded_input = encoded.as_slice();
        let redecoded = CanPlaceOnImpl::deserialize(&mut redecoded_input).unwrap();
        assert!(redecoded_input.is_empty());
        assert_eq!(redecoded, decoded);
    }

    #[test]
    fn nested_attribute_modifier_exact_matcher_round_trips_wire_and_type() {
        let mut wire = Vec::new();
        wire.write_var_int(&VarInt(1)).unwrap();
        wire.write_bool(false).unwrap();
        wire.write_bool(false).unwrap();
        wire.write_bool(false).unwrap();
        wire.write_var_int(&VarInt(1)).unwrap();
        wire.write_var_int(&VarInt(DataComponent::AttributeModifiers.to_id() as i32))
            .unwrap();
        wire.write_var_int(&VarInt(1)).unwrap();
        wire.write_var_int(&VarInt(3)).unwrap();
        wire.write_string("minecraft:nested_exact").unwrap();
        wire.write_f64(1.25).unwrap();
        wire.write_var_int(&VarInt(0)).unwrap();
        wire.write_var_int(&VarInt(0)).unwrap();
        wire.write_var_int(&VarInt(0)).unwrap();
        wire.write_var_int(&VarInt(0)).unwrap();

        let mut input = wire.as_slice();
        let decoded = CanPlaceOnImpl::deserialize(&mut input).unwrap();
        assert!(input.is_empty());
        let predicate = decoded.predicate.extract_list().unwrap()[0]
            .extract_compound()
            .unwrap();
        let components = predicate
            .get_compound("components")
            .unwrap()
            .get_compound("components")
            .unwrap();
        let modifiers = components
            .get_list("minecraft:attribute_modifiers")
            .unwrap();
        assert_eq!(modifiers.len(), 1);
        assert_eq!(
            modifiers[0].extract_compound().unwrap().get_string("type"),
            Some("minecraft:attack_damage")
        );

        let mut encoded = Vec::new();
        decoded.serialize(&mut encoded).unwrap();
        assert_eq!(encoded, wire);
    }

    #[test]
    fn can_break_rejects_negative_and_unknown_component_ids() {
        let mut negative = &[0xff, 0xff, 0xff, 0xff, 0x0f][..];
        assert!(CanBreakImpl::deserialize(&mut negative).is_err());
        let unknown = vec![1, 0, 0, 0, 1, 0xff, 0xff, 0xff, 0x0f];
        let mut input = unknown.as_slice();
        assert!(CanBreakImpl::deserialize(&mut input).is_err());
    }

    #[test]
    fn empty_predicate_lists_keep_the_official_zero_count_wire() {
        let mut place_on = Vec::new();
        CanPlaceOnImpl {
            predicate: NbtTag::List(Vec::new()),
        }
        .serialize(&mut place_on)
        .unwrap();
        assert_eq!(place_on, [0]);

        let mut breaks = Vec::new();
        CanBreakImpl {
            predicate: NbtTag::List(Vec::new()),
        }
        .serialize(&mut breaks)
        .unwrap();
        assert_eq!(breaks, [0]);
    }

    #[test]
    fn adventure_predicate_limits_and_write_errors_are_rejected() {
        let mut too_many = Vec::new();
        too_many.write_var_int(&VarInt(257)).unwrap();
        let mut input = too_many.as_slice();
        assert!(CanPlaceOnImpl::deserialize(&mut input).is_err());

        let mut unknown_block = NbtCompound::new();
        unknown_block.put_string("blocks", "minecraft:not_a_block".to_owned());
        let mut output = Vec::new();
        assert!(
            CanBreakImpl {
                predicate: NbtTag::Compound(unknown_block),
            }
            .serialize(&mut output)
            .is_err()
        );

        let mut unknown_component = NbtCompound::new();
        unknown_component.put_string("minecraft:not_a_component", "value".to_owned());
        let mut components = NbtCompound::new();
        components.put("components", NbtTag::Compound(unknown_component));
        let mut predicate = NbtCompound::new();
        predicate.put("components", NbtTag::Compound(components));
        let mut output = Vec::new();
        assert!(
            CanPlaceOnImpl {
                predicate: NbtTag::Compound(predicate),
            }
            .serialize(&mut output)
            .is_err()
        );
    }
}
