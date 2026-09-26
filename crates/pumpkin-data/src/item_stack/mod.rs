use crate::data_component::DataComponent;
use crate::data_component::DataComponent::Enchantments;
use crate::data_component_impl::{
    BlocksAttacksImpl, ConsumableImpl, CustomDataImpl, DamageImpl, DataComponentImpl,
    EnchantmentsImpl, IDSet, MaxDamageImpl, MaxStackSizeImpl, Rarity, RarityImpl,
    SwingAnimationImpl, ToolImpl, UnbreakableImpl, UseCooldownImpl, get, get_mut, read_data,
};

use crate::item::Item;
use crate::recipes::RecipeResultStruct;
use crate::tag::Taggable;
use crate::{Block, Enchantment};
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_nbt::tag::NbtTag;
use pumpkin_util::GameMode;
use rand;
use std::borrow::Cow;
use std::cmp::{max, min};
use std::num::NonZero;
use std::sync::atomic::{AtomicU32, Ordering};

mod categories;

/// The outcome of a [`ItemStack::damage_item`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamageResult {
    /// No damage was applied (zero/negative amount, not damageable, unbreakable,
    /// or Unbreaking negated every point).
    Untouched,
    /// Damage was applied and the item is still alive.
    Damaged,
    /// The item broke: one item was consumed from the stack (durability reset to 0),
    /// or the stack is now empty if it had only one item. Callers should always
    /// broadcast the break status — the client handles both cases correctly.
    Broken,
}

#[derive(Clone)]
pub struct ItemStack {
    pub item_count: u8,
    pub item: &'static Item,
    pub patch: Vec<(DataComponent, Option<Box<dyn DataComponentImpl>>)>,

    // unique ID for Bedrock network; don't serialize
    // Should always be a positive value for non-empty stacks
    pub uid: NonZero<i32>,
}

// impl Hash for ItemStack {
//     fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
//         self.item_count.hash(state);
//         self.item.id.hash(state);
//         self.patch.hash(state);
//     }
// }

/*
impl PartialEq for ItemStack {
    fn eq(&self, other: &Self) -> bool {
        self.item.id == other.item.id
    }
} */

pub struct ItemStackIdGenerator {
    counter: AtomicU32,
}

impl ItemStackIdGenerator {
    pub const fn new() -> Self {
        Self {
            counter: AtomicU32::new(1),
        }
    }

    pub fn next_id(&self) -> NonZero<i32> {
        // Wraps on overflow, which is what we want.
        let value = self.counter.fetch_add(1, Ordering::Relaxed);

        // Negative values are invalid; cycle through the positives
        let masked = value & 0x7FFFFFFF;

        if let Some(id) = NonZero::new(masked as i32) {
            id
        } else {
            // If we fetched 0 or 0x80000000, that's masked out as 0
            // Zero is invalid, so we just ask for the next ID to keep things simple/correct.
            self.next_id()
        }
    }
}

impl Default for ItemStackIdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

static ITEM_STACK_ID_GEN: ItemStackIdGenerator = ItemStackIdGenerator::new();

/// 26.2 built-in item metadata currently available in Pumpkin. The generated
/// 26.2 registry contains only vanilla-required built-ins; custom/non-vanilla
/// required-feature metadata remains an explicit follow-up instead of being
/// inferred from components, tags, or item names.
const VANILLA_REQUIRED_FEATURES: &[&str] = &["minecraft:vanilla"];

impl Item {
    #[must_use]
    pub const fn required_features(&self) -> &'static [&'static str] {
        VANILLA_REQUIRED_FEATURES
    }
}

impl ItemStack {
    #[must_use]
    pub fn new(item_count: u8, item: &'static Item) -> Self {
        Self {
            item_count,
            item,
            patch: Vec::new(),

            uid: ITEM_STACK_ID_GEN.next_id(),
        }
    }

    #[must_use]
    pub fn new_with_component(
        item_count: u8,
        item: &'static Item,
        component: Vec<(DataComponent, Option<Box<dyn DataComponentImpl>>)>,
    ) -> Self {
        Self {
            item_count,
            item,
            patch: component,

            uid: ITEM_STACK_ID_GEN.next_id(),
        }
    }

    /// Ignore the Bedrock UID for this `ItemStack`.
    /// This constructor is intended for Java-only item stacks, where the network UID
    /// is not required.
    #[must_use]
    pub const fn static_new_java(item_count: u8, item: &'static Item) -> Self {
        Self {
            item_count,
            item,
            patch: Vec::new(),

            uid: match NonZero::new(1) {
                Some(v) => v,
                None => panic!("1 is non-zero"),
            },
        }
    }

    /// Mirrors 26.2 `ItemStack.isItemEnabled`: empty stacks are enabled and
    /// every required feature must be present in the server's enabled set.
    #[must_use]
    pub fn is_item_enabled(&self, enabled_features: &[&str]) -> bool {
        self.is_empty()
            || self
                .item
                .required_features()
                .iter()
                .all(|required| enabled_features.contains(required))
    }

    #[must_use]
    pub fn get_data_component<T: DataComponentImpl + 'static>(&self) -> Option<&T> {
        let to_get_id = &T::get_enum();
        for (id, component) in self.patch.iter().rev() {
            if id == to_get_id {
                return component
                    .as_ref()
                    .map(|component| get::<T>(component.as_ref()));
            }
        }
        for (id, component) in self.item.components {
            if id == to_get_id {
                return Some(get::<T>(*component));
            }
        }
        None
    }
    fn normalize_patch_entry(&mut self, id: DataComponent) {
        let Some(first) = self.patch.iter().position(|(patch_id, _)| *patch_id == id) else {
            return;
        };
        let Some(last) = self.patch.iter().rposition(|(patch_id, _)| *patch_id == id) else {
            return;
        };
        let value = self.patch[last].1.take();
        for index in (first + 1..self.patch.len()).rev() {
            if self.patch[index].0 == id {
                self.patch.remove(index);
            }
        }
        self.patch[first].1 = value;
    }

    fn replace_patch_entry(
        &mut self,
        id: DataComponent,
        value: Option<Box<dyn DataComponentImpl>>,
    ) {
        self.normalize_patch_entry(id);
        if let Some((_, current)) = self.patch.iter_mut().find(|(patch_id, _)| *patch_id == id) {
            *current = value;
        } else {
            self.patch.push((id, value));
        }
    }

    #[must_use]
    pub fn get_data_component_mut<T: DataComponentImpl + 'static>(&mut self) -> Option<&mut T> {
        let to_get_id = T::get_enum();
        self.normalize_patch_entry(to_get_id);
        if let Some(index) = self.patch.iter().rposition(|(id, _)| *id == to_get_id) {
            return self.patch[index]
                .1
                .as_mut()
                .map(|component| get_mut::<T>(component.as_mut()));
        }

        // If not in patch, clone from item to patch and return mut
        let mut cloned = None;
        for (id, component) in self.item.components {
            if *id == to_get_id {
                cloned = Some((*id, Some(component.clone_dyn())));
                break;
            }
        }
        if let Some((id, component)) = cloned {
            self.patch.push((id, component));
            return self
                .patch
                .last_mut()?
                .1
                .as_mut()
                .map(|c| get_mut::<T>(c.as_mut()));
        }
        None
    }

    #[must_use]
    pub fn has_data_component(&self, to_get_id: DataComponent) -> bool {
        for (id, component) in self.patch.iter().rev() {
            if *id == to_get_id {
                return component.is_some();
            }
        }
        for (id, _) in self.item.components {
            if *id == to_get_id {
                return true;
            }
        }
        false
    }

    pub fn has_enchantments(&self) -> bool {
        self.get_data_component::<EnchantmentsImpl>()
            .is_some_and(|e| !e.enchantment.is_empty())
    }

    pub fn add_enchantment(&mut self, enchantment: &'static Enchantment, level: u16) {
        if let Some(enchantments) = self.get_data_component_mut::<EnchantmentsImpl>() {
            let mut new_vec = enchantments.enchantment.to_vec();
            new_vec.push((enchantment, level as i32));
            enchantments.enchantment = Cow::Owned(new_vec);
        } else {
            let enchantments = EnchantmentsImpl {
                enchantment: Cow::Owned(vec![(enchantment, level as i32)]),
            };
            self.replace_patch_entry(DataComponent::Enchantments, Some(Box::new(enchantments)));
        }
    }

    pub fn set_lore(&mut self, lines: Vec<pumpkin_util::text::TextComponent>) {
        self.replace_patch_entry(
            DataComponent::Lore,
            Some(Box::new(crate::data_component_impl::LoreImpl { lines })),
        );
    }

    pub fn set_data_component<T: DataComponentImpl + 'static>(&mut self, component: T) {
        self.replace_patch_entry(T::get_enum(), Some(Box::new(component)));
    }

    pub fn remove_data_component(&mut self, to_remove_id: DataComponent) {
        self.replace_patch_entry(to_remove_id, None);
    }

    pub fn add_lore(&mut self, line: pumpkin_util::text::TextComponent) {
        let mut lines = self
            .get_data_component::<crate::data_component_impl::LoreImpl>()
            .map_or_else(Vec::new, |lore| lore.lines.clone());
        lines.push(line);
        self.set_lore(lines);
    }

    pub const EMPTY: &'static Self = &Self {
        item_count: 0,
        item: &Item::AIR,
        patch: Vec::new(),

        uid: NonZero::<i32>::MIN, // white lie - Bedrock `uid` is never sent if the stack is empty
    };

    #[must_use]
    pub fn split_off(&mut self, amount: u8) -> Self {
        let count = amount.min(self.item_count);
        let result = self.copy_with_count(count);
        self.decrement(count);
        result
    }

    #[must_use]
    pub fn get_max_stack_size(&self) -> u8 {
        self.get_data_component::<MaxStackSizeImpl>()
            .map_or(1, |value| value.size)
    }

    #[must_use]
    pub fn get_max_damage(&self) -> Option<i32> {
        self.get_data_component::<MaxDamageImpl>()
            .map(|value| value.max_damage)
    }

    #[must_use]
    pub fn get_use_cooldown(&self) -> Option<&UseCooldownImpl> {
        self.get_data_component::<UseCooldownImpl>()
    }

    #[must_use]
    pub fn get_damage(&self) -> i32 {
        self.get_data_component::<DamageImpl>()
            .map_or(0, |value| value.damage)
    }

    #[must_use]
    pub fn get_enchantment_level(&self, enchantment: &'static Enchantment) -> i32 {
        let Some(data) = self.get_data_component::<EnchantmentsImpl>() else {
            return 0;
        };
        for (enc, level) in data.enchantment.iter() {
            if *enc == enchantment {
                return *level;
            }
        }
        0
    }

    #[must_use]
    pub fn is_unbreakable(&self) -> bool {
        self.get_data_component::<UnbreakableImpl>().is_some()
    }

    pub fn set_damage(&mut self, damage: i32) {
        let damage = damage.max(0);
        if damage == 0 {
            self.patch.retain(|(id, _)| *id != DataComponent::Damage);
            return;
        }

        self.replace_patch_entry(DataComponent::Damage, Some(DamageImpl { damage }.to_dyn()));
    }

    #[must_use]
    pub fn is_damageable(&self) -> bool {
        self.get_max_damage().unwrap_or(0) > 0
    }

    pub fn repair_item(&mut self, amount: i32) -> i32 {
        if amount <= 0 {
            return 0;
        }
        let damage = self.get_damage();
        if damage <= 0 {
            return 0;
        }
        let repaired = amount.min(damage);
        self.set_damage(damage - repaired);
        repaired
    }

    /// Core logic: apply Unbreaking chance with precomputed armor category and level.
    /// Extracted for use in damage_item where these values are hoisted outside the loop.
    /// Private to prevent incorrect usage; only call through damage_item.
    fn should_apply_durability_damage_with(is_armor: bool, unbreaking_level: i32) -> bool {
        if unbreaking_level <= 0 {
            return true;
        }

        // `#minecraft:enchantable/armor` uses the armor formula; all others use the tool formula.
        if is_armor {
            let chance = 0.6 + (0.4 / (unbreaking_level as f32 + 1.0));
            rand::random::<f32>() < chance
        } else {
            rand::random::<u32>().is_multiple_of(unbreaking_level as u32 + 1)
        }
    }

    /// Apply durability damage to this item and return the outcome.
    /// Callers must check the return value to handle break broadcasts and item stack updates.
    /// TODO: Restore `#[must_use]` once all callsites (esp. tool/mob block-hit/damage sites)
    /// implement proper `DamageResult::Broken` handling instead of suppressing with `let _ =`.
    /// Without this enforcement, the fix is incomplete vs vanilla break behavior.
    #[must_use]
    pub fn damage_item(&mut self, amount: i32) -> DamageResult {
        if amount <= 0 || !self.is_damageable() || self.is_unbreakable() {
            return DamageResult::Untouched;
        }

        let max_damage = self.get_max_damage().unwrap_or(0);
        if max_damage <= 0 {
            return DamageResult::Untouched;
        }

        // Hoist armor check and enchantment level outside loop to avoid repeated lookups.
        let is_armor = self.is_armor();
        let unbreaking_level = self.get_enchantment_level(&Enchantment::UNBREAKING);
        let mut applied = 0;
        // TODO: Short-circuit once applied >= (max_damage - current_damage) to avoid
        // iterating the full amount for high-damage hits on high-durability items.
        for _ in 0..amount {
            if Self::should_apply_durability_damage_with(is_armor, unbreaking_level) {
                applied += 1;
            }
        }

        if applied <= 0 {
            return DamageResult::Untouched;
        }

        let new_damage = self.get_damage().saturating_add(applied);
        if new_damage >= max_damage {
            // Vanilla behavior: breaking consumes one item from the stack and resets
            // durability to 0. A single damage call never breaks more than one item,
            // regardless of the damage amount. This matches vanilla item stack behavior.
            if self.item_count > 1 {
                self.item_count = self.item_count.saturating_sub(1);
                self.set_damage(0);
            } else {
                *self = Self::EMPTY.clone();
            }
            return DamageResult::Broken;
        }

        self.set_damage(new_damage);
        DamageResult::Damaged
    }

    #[must_use]
    pub fn get_max_use_time(&self) -> i32 {
        if let Some(value) = self.get_data_component::<ConsumableImpl>() {
            return value.consume_ticks();
        }
        if self.get_data_component::<BlocksAttacksImpl>().is_some() {
            return 72000;
        }
        0
    }

    #[must_use]
    pub const fn get_item(&self) -> &'static Item {
        if self.is_empty() {
            &Item::AIR
        } else {
            self.item
        }
    }

    #[must_use]
    pub fn is_stackable(&self) -> bool {
        self.get_max_stack_size() > 1 // TODO: && (!this.isDamageable() || !this.isDamaged());
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.item_count == 0 || self.item.id == Item::AIR.id
    }

    pub fn set_custom_name(&mut self, name: String) {
        use crate::data_component_impl::CustomNameImpl;
        let component = Some(
            CustomNameImpl {
                name: pumpkin_util::text::TextComponent::text(name),
            }
            .to_dyn(),
        );
        self.replace_patch_entry(DataComponent::CustomName, component);
    }

    #[must_use]
    pub fn has_custom_name(&self) -> bool {
        self.get_data_component::<crate::data_component_impl::CustomNameImpl>()
            .is_some()
    }

    #[must_use]
    pub fn get_custom_name(&self) -> Option<&pumpkin_util::text::TextComponent> {
        self.get_data_component::<crate::data_component_impl::CustomNameImpl>()
            .map(|c| &c.name)
    }

    pub fn remove_custom_name(&mut self) {
        self.patch
            .retain(|(id, _)| *id != DataComponent::CustomName);
    }

    #[must_use]
    pub fn get_hover_name(&self) -> String {
        if let Some(custom_name) =
            self.get_data_component::<crate::data_component_impl::CustomNameImpl>()
        {
            return custom_name.name.clone().get_text();
        }
        if let Some(item_name) =
            self.get_data_component::<crate::data_component_impl::ItemNameImpl>()
        {
            return item_name.name.as_component().get_text();
        }
        self.item.registry_key.to_string()
    }

    #[must_use]
    pub fn get_repair_cost(&self) -> i32 {
        self.get_data_component::<crate::data_component_impl::RepairCostImpl>()
            .map_or(0, |value| value.cost)
    }

    pub fn set_repair_cost(&mut self, cost: i32) {
        if cost <= 0 {
            self.patch
                .retain(|(id, _)| *id != DataComponent::RepairCost);
            return;
        }
        self.set_data_component(crate::data_component_impl::RepairCostImpl { cost });
    }

    #[must_use]
    pub fn is_valid_repair_item(&self, repair_item: &ItemStack) -> bool {
        let repairable = self.get_data_component::<crate::data_component_impl::RepairableImpl>();
        repairable.is_some_and(|r| r.is_valid_repair_item(repair_item))
    }

    #[must_use]
    pub fn get_swing_animation(&self) -> SwingAnimationImpl {
        self.get_data_component::<SwingAnimationImpl>()
            .copied()
            .unwrap_or(SwingAnimationImpl::DEFAULT)
    }

    #[must_use]
    pub fn get_rarity(&self) -> Rarity {
        let base = self
            .get_data_component::<RarityImpl>()
            .map_or(Rarity::Common, |r| r.rarity);
        if !self.has_enchantments() {
            return base;
        }
        match base {
            Rarity::Common | Rarity::Uncommon => Rarity::Rare,
            Rarity::Rare => Rarity::Epic,
            Rarity::Epic => Rarity::Epic,
        }
    }

    #[must_use]
    pub fn custom_data_compound(&self) -> Option<&NbtCompound> {
        self.get_data_component::<CustomDataImpl>()
            .map(|custom_data| &custom_data.data)
    }

    pub fn set_custom_data(&mut self, namespace: &str, key: &str, value: NbtTag) {
        let mut custom_data = self
            .get_data_component::<CustomDataImpl>()
            .map_or_else(NbtCompound::new, |custom_data| custom_data.data.clone());

        let mut namespace_data = custom_data
            .child_tags
            .remove(namespace)
            .and_then(|tag| match tag {
                NbtTag::Compound(compound) => Some(compound),
                _ => None,
            })
            .unwrap_or_default();

        namespace_data.child_tags.insert(key.into(), value);
        custom_data
            .child_tags
            .insert(namespace.into(), NbtTag::Compound(namespace_data));

        self.set_custom_data_component(custom_data);
    }

    fn set_custom_data_component(&mut self, custom_data: NbtCompound) {
        self.replace_patch_entry(
            DataComponent::CustomData,
            Some(CustomDataImpl { data: custom_data }.to_dyn()),
        );
    }

    pub fn get_custom_data(&self, namespace: &str, key: &str) -> Option<NbtTag> {
        self.custom_data_compound()?
            .get(namespace)?
            .extract_compound()?
            .get(key)
            .cloned()
    }

    pub fn remove_custom_data(&mut self, namespace: &str, key: &str) {
        let Some(mut custom_data) = self
            .get_data_component::<CustomDataImpl>()
            .map(|custom_data| custom_data.data.clone())
        else {
            return;
        };

        let Some(NbtTag::Compound(mut namespace_data)) = custom_data.child_tags.remove(namespace)
        else {
            return;
        };

        namespace_data.child_tags.remove(key);
        if !namespace_data.is_empty() {
            custom_data
                .child_tags
                .insert(namespace.into(), NbtTag::Compound(namespace_data));
        }

        if custom_data.is_empty() {
            self.patch
                .retain(|(id, _)| *id != DataComponent::CustomData);
        } else {
            self.set_custom_data_component(custom_data);
        }
    }

    #[must_use]
    pub fn has_custom_data(&self, namespace: &str, key: &str) -> bool {
        self.get_custom_data(namespace, key).is_some()
    }

    #[must_use]
    pub fn split(&mut self, amount: u8) -> Self {
        let min = amount.min(self.item_count);
        let stack = self.copy_with_count(min);
        self.decrement(min);
        stack
    }

    #[must_use]
    pub fn split_unless_creative(&mut self, gamemode: GameMode, amount: u8) -> Self {
        let min = amount.min(self.item_count);
        let stack = self.copy_with_count(min);
        if gamemode != GameMode::Creative {
            self.decrement(min);
        }
        stack
    }

    #[must_use]
    pub fn copy_with_count(&self, count: u8) -> Self {
        let mut stack = self.clone();
        stack.uid = ITEM_STACK_ID_GEN.next_id();
        stack.item_count = count;
        stack
    }

    pub const fn set_count(&mut self, count: u8) {
        self.item_count = count;
    }

    pub fn decrement_unless_creative(&mut self, gamemode: GameMode, amount: u8) {
        if gamemode != GameMode::Creative {
            self.item_count = self.item_count.saturating_sub(amount);
            if self.item_count == 0 {
                self.clear();
            }
        }
    }

    pub const fn decrement(&mut self, amount: u8) {
        self.item_count = self.item_count.saturating_sub(amount);
    }

    pub const fn increment(&mut self, amount: u8) {
        self.item_count = self.item_count.saturating_add(amount);
    }

    /// Completely resets the stack to air
    pub fn clear(&mut self) {
        *self = Self::EMPTY.clone();
    }

    pub fn enchant(&mut self, enchantment: &'static Enchantment, level: i32) {
        if level <= 0 {
            return;
        }
        let level = min(level, 255);
        if let Some(data) = self.get_data_component_mut::<EnchantmentsImpl>() {
            for (enc, old_level) in data.enchantment.to_mut() {
                if *enc == enchantment {
                    *old_level = max(*old_level, level);
                    return;
                }
            }
            data.enchantment.to_mut().push((enchantment, level));
        } else {
            self.set_data_component(EnchantmentsImpl {
                enchantment: Cow::Owned(vec![(enchantment, level)]),
            });
        }
    }

    #[must_use]
    pub fn effective_patch(&self) -> Vec<(DataComponent, Option<&dyn DataComponentImpl>)> {
        let mut effective = Vec::new();
        for (id, _) in &self.patch {
            if effective.iter().any(|(seen_id, _)| seen_id == id) {
                continue;
            }
            let value = self
                .patch
                .iter()
                .rev()
                .find(|(patch_id, _)| patch_id == id)
                .and_then(|(_, value)| value.as_deref());
            effective.push((*id, value));
        }
        effective
    }

    fn effective_patch_value(&self, id: DataComponent) -> Option<Option<&dyn DataComponentImpl>> {
        self.patch
            .iter()
            .rev()
            .find(|(patch_id, _)| *patch_id == id)
            .map(|(_, value)| value.as_deref())
    }

    #[must_use]
    pub fn are_items_and_components_equal(&self, other: &Self) -> bool {
        if self.item != other.item {
            return false;
        }

        let mut ids = Vec::new();
        for (id, _) in self.patch.iter().chain(&other.patch) {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        ids.into_iter().all(|id| {
            match (
                self.effective_patch_value(id),
                other.effective_patch_value(id),
            ) {
                (None, None) => true,
                (Some(None), Some(None)) => true,
                (Some(Some(left)), Some(Some(right))) => left.equal(right),
                _ => false,
            }
        })
    }

    #[must_use]
    pub fn are_equal(&self, other: &Self) -> bool {
        self.item_count == other.item_count && self.are_items_and_components_equal(other)
    }

    #[must_use]
    pub fn get_hash(&self) -> i32 {
        let mut digest = crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc32Iscsi);
        digest.update(&self.item.id.to_le_bytes());
        digest.update(&[self.item_count]);
        let mut values = Vec::new();
        for (id, _) in &self.patch {
            if !values.iter().any(|(seen_id, _, _)| *seen_id == *id) {
                if let Some(value) = self.effective_patch_value(*id) {
                    values.push((
                        *id,
                        value.is_some(),
                        value.map_or(0, |value| value.get_hash()),
                    ));
                }
            }
        }
        values.sort_unstable_by_key(|(id, _, _)| id.to_id());
        for (id, present, hash) in values {
            digest.update(&[id.to_id(), u8::from(present)]);
            digest.update(&hash.to_le_bytes());
        }
        digest.finalize() as i32
    }

    /// Determines the mining speed for a block based on tool rules.
    /// Direct matches return immediately, tagged blocks are checked separately.
    /// If no match is found, returns the tool's default mining speed or `1.0`.
    #[must_use]
    pub fn get_speed(&self, block: &'static Block) -> f32 {
        // No tool? Use default speed
        if let Some(tool) = self.get_data_component::<ToolImpl>() {
            for rule in tool.rules.iter() {
                // Skip if speed is not set
                let Some(speed) = rule.speed else {
                    continue;
                };
                match &rule.blocks {
                    IDSet::Tag(tag) => {
                        if block.is_tagged_with(tag).unwrap_or(false) {
                            return speed;
                        }
                    }
                    IDSet::IDs(blocks) => {
                        if blocks.contains(&block) {
                            return speed;
                        }
                    }
                }
            }
            tool.default_mining_speed
        } else {
            1.0
        }
    }

    /// Determines if a tool is valid for block drops based on tool rules.
    /// Direct matches return immediately, while tagged blocks are checked separately.
    #[must_use]
    pub fn is_correct_for_drops(&self, block: &'static Block) -> bool {
        if let Some(tool) = self.get_data_component::<ToolImpl>() {
            for rule in tool.rules.iter() {
                // Skip if speed is not set
                let Some(correct) = rule.correct_for_drops else {
                    continue;
                };
                match &rule.blocks {
                    IDSet::Tag(tag) => {
                        if block.is_tagged_with(tag).unwrap_or(false) {
                            return correct;
                        }
                    }
                    IDSet::IDs(blocks) => {
                        if blocks.contains(&block) {
                            return correct;
                        }
                    }
                }
            }
        }
        false
    }

    pub fn write_item_stack(&self, compound: &mut NbtCompound) {
        // Minecraft 1.21.4 uses "id" as string with namespaced ID (minecraft:diamond_sword)
        compound.put_string("id", format!("minecraft:{}", self.item.registry_key));
        compound.put_int("count", self.item_count as i32);

        // Create a tag compound for additional data
        let mut tag = NbtCompound::new();

        for (id, data) in self.effective_patch() {
            if let Some(data) = data {
                tag.put(id.to_name(), data.write_data());
            } else {
                let name = '!'.to_string() + id.to_name();
                tag.put(name.as_str(), NbtCompound::new());
            }
        }

        // Store custom data like enchantments, display name, etc. would go here
        compound.put_compound("components", tag);
    }

    #[must_use]
    pub fn read_item_stack(compound: &NbtCompound) -> Option<Self> {
        // Get ID, which is a string like "minecraft:diamond_sword".
        let full_id = compound.get_string("id")?;
        let registry_key = full_id.strip_prefix("minecraft:").unwrap_or(full_id);
        let item = Item::from_registry_key(registry_key)?;
        if item.id == Item::AIR.id {
            return None;
        }

        // ItemStack.CODEC defaults a missing count to one and accepts 1..=99.
        let count = match compound.child_tags.get("count") {
            None => 1,
            Some(count) => u8::try_from(crate::data_component_impl::food::nbt_i32(count)?)
                .ok()
                .filter(|count| (1..=99).contains(count))?,
        };

        let mut item_stack = Self::new(count, item);

        // components is optional, but a present value must be a compound.
        if let Some(components) = compound.child_tags.get("components") {
            let tag = components.extract_compound()?;
            for (name, data) in &tag.child_tags {
                if let Some(name) = name.strip_prefix("!") {
                    item_stack
                        .patch
                        .push((DataComponent::try_from_name(name)?, None));
                } else {
                    let id = DataComponent::try_from_name(name)?;
                    item_stack.patch.push((id, Some(read_data(id, data)?)));
                }
            }
        }

        Some(item_stack)
    }

    /// Reads the ItemStackTemplate NBT alternative: a bare item identifier is
    /// count one with an empty component patch; ordinary ItemStack callers stay
    /// compound-only and retain their optional-empty semantics.
    #[must_use]
    pub fn read_item_stack_template(data: &NbtTag) -> Option<Self> {
        match data {
            NbtTag::String(id) => {
                let registry_key = id.strip_prefix("minecraft:").unwrap_or(id);
                let item = Item::from_registry_key(registry_key)?;
                (item.id != Item::AIR.id).then(|| Self::new(1, item))
            }
            NbtTag::Compound(compound) => Self::read_item_stack(compound),
            _ => None,
        }
    }
}

impl From<&RecipeResultStruct> for ItemStack {
    fn from(value: &RecipeResultStruct) -> Self {
        Self::new(
            value.count,
            Item::from_registry_key(value.id.strip_prefix("minecraft:").unwrap_or(value.id))
                .unwrap_or(&Item::AIR),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_component::DataComponent;
    use crate::data_component_impl::{
        BundleContentsImpl, ConsumableImpl, ContainerImpl, CustomDataImpl, CustomNameImpl,
        DamageImpl, DataComponentImpl, EnchantmentsImpl, ItemNameImpl, JukeboxPlayableImpl,
        LoreImpl, MapDecorationsImpl, RecipesImpl, UnbreakableImpl, UseRemainderImpl, get,
    };

    /// Helper: creates a fresh Iron Sword (max_damage 250, damage 0).
    fn iron_sword() -> ItemStack {
        ItemStack::new(1, &Item::IRON_SWORD)
    }

    #[test]
    fn map_decorations_item_stack_nbt_round_trip_preserves_entries() {
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

        let mut decorations = NbtCompound::new();
        decorations.put_compound("player", player);
        decorations.put_compound("custom marker", custom);
        let mut components = NbtCompound::new();
        components.put(
            "minecraft:map_decorations",
            NbtTag::Compound(decorations.clone()),
        );

        let mut input = NbtCompound::new();
        input.put_string("id", "minecraft:filled_map".to_owned());
        input.put_int("count", 1);
        input.put_compound("components", components.clone());

        let decoded = ItemStack::read_item_stack(&input).expect("map stack should decode");
        let map = decoded
            .get_data_component::<MapDecorationsImpl>()
            .expect("map decorations should decode");
        assert_eq!(map.decorations.len(), 2);

        let mut output = NbtCompound::new();
        decoded.write_item_stack(&mut output);
        assert_eq!(output.get_compound("components"), Some(&components));
    }

    #[test]
    fn death_protection_item_stack_nbt_round_trip_preserves_effects() {
        let mut effect = NbtCompound::new();
        effect.put_string("type", "minecraft:clear_all_effects".to_owned());
        let mut death_protection = NbtCompound::new();
        death_protection.put_list("death_effects", vec![NbtTag::Compound(effect)]);
        let mut components = NbtCompound::new();
        components.put(
            "minecraft:death_protection",
            NbtTag::Compound(death_protection),
        );

        let mut input = NbtCompound::new();
        input.put_string("id", "minecraft:totem_of_undying".to_owned());
        input.put_int("count", 1);
        input.put_compound("components", components.clone());

        let decoded = ItemStack::read_item_stack(&input).expect("stack should decode");
        let death_protection = decoded
            .get_data_component::<crate::data_component_impl::DeathProtectionImpl>()
            .expect("death protection should decode");
        assert_eq!(death_protection.death_effects.len(), 1);

        let mut output = NbtCompound::new();
        decoded.write_item_stack(&mut output);
        assert_eq!(output.get_compound("components"), Some(&components));
    }

    #[test]
    fn death_protection_nbt_applies_official_optional_defaults() {
        let absent = crate::data_component_impl::DeathProtectionImpl::read_data(&NbtTag::Compound(
            NbtCompound::new(),
        ))
        .expect("absent death_effects defaults to an empty list");
        assert!(absent.death_effects.is_empty());

        let mut status = NbtCompound::new();
        status.put_string("id", "minecraft:regeneration".to_owned());

        let mut apply = NbtCompound::new();
        apply.put_string("type", "minecraft:apply_effects".to_owned());
        apply.put_list("effects", vec![NbtTag::Compound(status)]);

        let mut teleport = NbtCompound::new();
        teleport.put_string("type", "minecraft:teleport_randomly".to_owned());
        let mut death_protection = NbtCompound::new();
        death_protection.put_list(
            "death_effects",
            vec![NbtTag::Compound(apply), NbtTag::Compound(teleport)],
        );
        let mut components = NbtCompound::new();
        components.put(
            "minecraft:death_protection",
            NbtTag::Compound(death_protection),
        );
        let mut input = NbtCompound::new();
        input.put_string("id", "minecraft:totem_of_undying".to_owned());
        input.put_int("count", 1);
        input.put_compound("components", components);

        let stack = ItemStack::read_item_stack(&input).expect("official optional fields decode");
        let protection = stack
            .get_data_component::<crate::data_component_impl::DeathProtectionImpl>()
            .expect("death protection component");
        let crate::data_component_impl::ConsumeEffect::ApplyEffects((effects, probability)) =
            &protection.death_effects[0]
        else {
            panic!("expected apply effects");
        };
        assert_eq!(*probability, 1.0);
        assert_eq!(effects[0].amplifier, 0);
        assert_eq!(effects[0].duration, 0);
        assert!(!effects[0].ambient);
        assert!(effects[0].show_particles);
        assert!(effects[0].show_icon);
        match &protection.death_effects[1] {
            crate::data_component_impl::ConsumeEffect::TeleportRandomly(diameter) => {
                assert!((*diameter - 16.0).abs() < f32::EPSILON);
            }
            _ => panic!("expected teleport effect"),
        }
    }

    #[test]
    fn death_protection_show_icon_defaults_to_show_particles() {
        let mut status = NbtCompound::new();
        status.put_string("id", "minecraft:regeneration".to_owned());
        status.put_bool("show_particles", false);
        let mut apply = NbtCompound::new();
        apply.put_string("type", "minecraft:apply_effects".to_owned());
        apply.put_list("effects", vec![NbtTag::Compound(status)]);
        let mut protection = NbtCompound::new();
        protection.put_list("death_effects", vec![NbtTag::Compound(apply)]);
        let decoded = crate::data_component_impl::DeathProtectionImpl::read_data(
            &NbtTag::Compound(protection),
        )
        .expect("optional show_icon should decode");
        let crate::data_component_impl::ConsumeEffect::ApplyEffects((effects, _)) =
            &decoded.death_effects[0]
        else {
            panic!("expected apply effects");
        };
        assert!(!effects[0].show_particles);
        assert!(!effects[0].show_icon);
    }

    #[test]
    fn death_protection_accepts_official_numeric_conversions() {
        let mut status = NbtCompound::new();
        status.put_string("id", "minecraft:regeneration".to_owned());
        status.put_byte("amplifier", 1);
        status.put_long("duration", 900);
        status.put_int("ambient", 0);
        status.put_short("show_particles", 1);
        status.put_int("show_icon", 1);
        let mut apply = NbtCompound::new();
        apply.put_string("type", "minecraft:apply_effects".to_owned());
        apply.put_int("probability", 1);
        apply.put_list("effects", vec![NbtTag::Compound(status)]);
        let mut teleport = NbtCompound::new();
        teleport.put_string("type", "minecraft:teleport_randomly".to_owned());
        teleport.put_int("diameter", 16);
        let mut protection = NbtCompound::new();
        protection.put_list(
            "death_effects",
            vec![NbtTag::Compound(apply), NbtTag::Compound(teleport)],
        );
        let decoded = crate::data_component_impl::DeathProtectionImpl::read_data(
            &NbtTag::Compound(protection),
        )
        .expect("numeric NBT values should use official number conversion");
        assert_eq!(decoded.death_effects.len(), 2);
    }

    #[test]
    fn death_protection_numeric_narrowing_matches_java_number() {
        let decode_duration = |tag| {
            let mut status = NbtCompound::new();
            status.put_string("id", "minecraft:regeneration".to_owned());
            status.put("duration", tag);
            crate::data_component_impl::StatusEffectInstance::read_data(&NbtTag::Compound(status))
                .expect("numeric duration should decode")
                .duration
        };

        assert_eq!(decode_duration(NbtTag::Long(i64::MAX)), -1);
        assert_eq!(decode_duration(NbtTag::Double(f64::INFINITY)), i32::MAX);
        assert_eq!(decode_duration(NbtTag::Double(f64::NEG_INFINITY)), i32::MIN);
        assert_eq!(decode_duration(NbtTag::Double(f64::NAN)), 0);
    }

    #[test]
    fn death_protection_rejects_wrong_types_in_optional_fields() {
        let mut status = NbtCompound::new();
        status.put_string("id", "minecraft:regeneration".to_owned());
        status.put_string("amplifier", "not a number".to_owned());
        let mut apply = NbtCompound::new();
        apply.put_string("type", "minecraft:apply_effects".to_owned());
        apply.put_list("effects", vec![NbtTag::Compound(status)]);
        let mut protection = NbtCompound::new();
        protection.put_list("death_effects", vec![NbtTag::Compound(apply)]);
        assert!(
            crate::data_component_impl::DeathProtectionImpl::read_data(&NbtTag::Compound(
                protection
            ))
            .is_none()
        );

        let mut apply = NbtCompound::new();
        apply.put_string("type", "minecraft:apply_effects".to_owned());
        apply.put_string("probability", "not a number".to_owned());
        apply.put_list("effects", Vec::new());
        let mut protection = NbtCompound::new();
        protection.put_list("death_effects", vec![NbtTag::Compound(apply)]);
        assert!(
            crate::data_component_impl::DeathProtectionImpl::read_data(&NbtTag::Compound(
                protection
            ))
            .is_none()
        );

        let mut teleport = NbtCompound::new();
        teleport.put_string("type", "minecraft:teleport_randomly".to_owned());
        teleport.put_string("diameter", "not a number".to_owned());
        let mut protection = NbtCompound::new();
        protection.put_list("death_effects", vec![NbtTag::Compound(teleport)]);
        assert!(
            crate::data_component_impl::DeathProtectionImpl::read_data(&NbtTag::Compound(
                protection
            ))
            .is_none()
        );

        let mut status = NbtCompound::new();
        status.put_string("id", "minecraft:regeneration".to_owned());
        status.put_string("ambient", "not a boolean".to_owned());
        let mut apply = NbtCompound::new();
        apply.put_string("type", "minecraft:apply_effects".to_owned());
        apply.put_list("effects", vec![NbtTag::Compound(status)]);
        let mut protection = NbtCompound::new();
        protection.put_list("death_effects", vec![NbtTag::Compound(apply)]);
        assert!(
            crate::data_component_impl::DeathProtectionImpl::read_data(&NbtTag::Compound(
                protection
            ))
            .is_none()
        );
    }

    #[test]
    fn death_protection_rejects_values_outside_official_codecs() {
        let mut apply = NbtCompound::new();
        apply.put_string("type", "minecraft:apply_effects".to_owned());
        apply.put_float("probability", 1.01);
        apply.put_list("effects", Vec::new());
        let mut protection = NbtCompound::new();
        protection.put_list("death_effects", vec![NbtTag::Compound(apply)]);
        assert!(
            crate::data_component_impl::DeathProtectionImpl::read_data(&NbtTag::Compound(
                protection
            ))
            .is_none()
        );

        for probability in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut apply = NbtCompound::new();
            apply.put_string("type", "minecraft:apply_effects".to_owned());
            apply.put_float("probability", probability);
            apply.put_list("effects", Vec::new());
            let mut protection = NbtCompound::new();
            protection.put_list("death_effects", vec![NbtTag::Compound(apply)]);
            assert!(
                crate::data_component_impl::DeathProtectionImpl::read_data(&NbtTag::Compound(
                    protection
                ))
                .is_none()
            );
        }

        let mut teleport = NbtCompound::new();
        teleport.put_string("type", "minecraft:teleport_randomly".to_owned());
        teleport.put_float("diameter", 0.0);
        let mut protection = NbtCompound::new();
        protection.put_list("death_effects", vec![NbtTag::Compound(teleport)]);
        assert!(
            crate::data_component_impl::DeathProtectionImpl::read_data(&NbtTag::Compound(
                protection
            ))
            .is_none()
        );

        let mut status = NbtCompound::new();
        status.put_string("id", "minecraft:regeneration".to_owned());
        status.put_int("amplifier", 256);
        let mut apply = NbtCompound::new();
        apply.put_string("type", "minecraft:apply_effects".to_owned());
        apply.put_list("effects", vec![NbtTag::Compound(status)]);
        let mut protection = NbtCompound::new();
        protection.put_list("death_effects", vec![NbtTag::Compound(apply)]);
        assert!(
            crate::data_component_impl::DeathProtectionImpl::read_data(&NbtTag::Compound(
                protection
            ))
            .is_none()
        );
    }

    #[test]
    fn consumable_rejects_invalid_inline_effect() {
        let mut invalid_effect = NbtCompound::new();
        invalid_effect.put_string("type", "minecraft:not_a_consume_effect".to_owned());
        let mut consumable = NbtCompound::new();
        consumable.put_float("consume_seconds", 1.6);
        consumable.put_string("animation", "eat".to_owned());
        consumable.put_string("sound", "minecraft:entity.generic.eat".to_owned());
        consumable.put_bool("has_consume_particles", true);
        consumable.put_list("on_consume_effects", vec![NbtTag::Compound(invalid_effect)]);

        assert!(ConsumableImpl::read_data(&NbtTag::Compound(consumable)).is_none());
    }

    #[test]
    fn jukebox_playable_nbt_round_trip_preserves_song() {
        let mut component = NbtCompound::new();
        component.put_string("song", "minecraft:13".to_owned());
        let mut components = NbtCompound::new();
        components.put(
            "minecraft:jukebox_playable",
            NbtTag::Compound(component.clone()),
        );
        let mut input = NbtCompound::new();
        input.put_string("id", "minecraft:music_disc_13".to_owned());
        input.put_int("count", 1);
        input.put_compound("components", components.clone());

        let stack = ItemStack::read_item_stack(&input).expect("jukebox component should decode");
        assert_eq!(
            stack
                .get_data_component::<JukeboxPlayableImpl>()
                .expect("jukebox component")
                .song,
            "minecraft:13"
        );

        let mut output = NbtCompound::new();
        stack.write_item_stack(&mut output);
        assert_eq!(output.get_compound("components"), Some(&components));
    }

    #[test]
    fn jukebox_playable_rejects_unknown_song_and_wrong_type() {
        let mut component = NbtCompound::new();
        component.put_string("song", "minecraft:not_a_song".to_owned());
        assert!(JukeboxPlayableImpl::read_data(&NbtTag::Compound(component)).is_none());
        assert!(JukeboxPlayableImpl::read_data(&NbtTag::String("minecraft:13".into())).is_none());
    }

    #[test]
    fn recipes_nbt_round_trip_preserves_identifier_list() {
        let input = NbtTag::List(vec![
            NbtTag::String("minecraft:iron_ingot_from_nuggets".into()),
            NbtTag::String("example:custom_recipe".into()),
        ]);
        let decoded = RecipesImpl::read_data(&input).expect("recipes list should decode");
        assert_eq!(
            decoded.recipes,
            vec![
                "minecraft:iron_ingot_from_nuggets".to_owned(),
                "example:custom_recipe".to_owned(),
            ]
        );
        assert_eq!(decoded.write_data(), input);

        let empty = NbtTag::List(Vec::new());
        assert_eq!(
            RecipesImpl::read_data(&empty)
                .expect("empty recipes list should decode")
                .write_data(),
            empty
        );
    }

    #[test]
    fn recipes_item_stack_nbt_round_trip_preserves_component_shape() {
        let mut components = NbtCompound::new();
        components.put_list(
            "minecraft:recipes",
            vec![NbtTag::String("minecraft:iron_ingot_from_nuggets".into())],
        );
        let mut input = NbtCompound::new();
        input.put_string("id", "minecraft:knowledge_book".to_owned());
        input.put_int("count", 1);
        input.put_compound("components", components.clone());

        let stack = ItemStack::read_item_stack(&input).expect("knowledge book should decode");
        assert_eq!(
            stack
                .get_data_component::<RecipesImpl>()
                .expect("recipes component")
                .recipes,
            vec!["minecraft:iron_ingot_from_nuggets".to_owned()]
        );
        let mut output = NbtCompound::new();
        stack.write_item_stack(&mut output);
        assert_eq!(output.get_compound("components"), Some(&components));
    }

    #[test]
    fn recipes_nbt_rejects_missing_invalid_and_wrong_entries() {
        assert!(RecipesImpl::read_data(&NbtTag::End).is_none());
        assert!(RecipesImpl::read_data(&NbtTag::Int(0)).is_none());
        assert!(RecipesImpl::read_data(&NbtTag::List(vec![NbtTag::Int(1)])).is_none());
        assert!(
            RecipesImpl::read_data(&NbtTag::List(vec![NbtTag::String("a:b:c".into())])).is_none()
        );
        assert!(
            RecipesImpl::read_data(&NbtTag::List(vec![NbtTag::String(
                "bad namespace:path".into()
            )]))
            .is_none()
        );
    }

    #[test]
    fn recipes_nbt_uses_official_identifier_parse_defaults() {
        let input = NbtTag::List(vec![
            NbtTag::String(":custom_recipe".into()),
            NbtTag::String("unqualified_recipe".into()),
            NbtTag::String("example:".into()),
            NbtTag::String(":".into()),
            NbtTag::String("".into()),
        ]);
        assert_eq!(
            RecipesImpl::read_data(&input).unwrap().recipes,
            vec![
                "minecraft:custom_recipe".to_owned(),
                "minecraft:unqualified_recipe".to_owned(),
                "example:".to_owned(),
                "minecraft:".to_owned(),
                "minecraft:".to_owned(),
            ]
        );
    }

    #[test]
    fn item_enabled_requires_all_required_features_without_mutating_stack() {
        let stack = ItemStack::new(1, &Item::DIAMOND_SWORD);
        assert!(stack.is_item_enabled(&["minecraft:vanilla"]));
        assert!(!stack.is_item_enabled(&[]));
        assert_eq!(stack.item_count, 1);
        assert!(stack.patch.is_empty());
    }

    #[test]
    fn empty_stack_is_enabled_without_feature_flags() {
        let stack = ItemStack::static_new_java(0, &Item::AIR);
        assert!(stack.is_empty());
        assert!(stack.is_item_enabled(&[]));
    }

    #[test]
    fn items_with_different_components_are_not_equal_in_either_direction() {
        let plain = ItemStack::new(1, &Item::COAL);

        let mut customized = ItemStack::new(1, &Item::COAL);
        customized
            .patch
            .push((DataComponent::Unbreakable, Some(UnbreakableImpl.to_dyn())));

        assert!(!plain.are_items_and_components_equal(&customized));
        assert!(!customized.are_items_and_components_equal(&plain));
        assert!(customized.are_items_and_components_equal(&customized.clone()));
    }

    #[test]
    fn duplicate_component_set_replaces_tombstone_for_all_model_views() {
        let mut stack = ItemStack::new_with_component(
            1,
            &Item::IRON_SWORD,
            vec![
                (
                    DataComponent::Damage,
                    Some(DamageImpl { damage: 3 }.to_dyn()),
                ),
                (DataComponent::Damage, None),
            ],
        );
        stack.set_data_component(DamageImpl { damage: 7 });

        let mut expected = ItemStack::new(1, &Item::IRON_SWORD);
        expected.set_data_component(DamageImpl { damage: 7 });
        assert_eq!(
            stack
                .get_data_component::<DamageImpl>()
                .map(|damage| damage.damage),
            Some(7)
        );
        assert!(stack.has_data_component(DataComponent::Damage));
        assert!(stack.are_items_and_components_equal(&expected));
        assert_eq!(stack.get_hash(), expected.get_hash());

        let mut encoded = NbtCompound::new();
        stack.write_item_stack(&mut encoded);
        let components = encoded
            .get_compound("components")
            .expect("components should be encoded");
        assert!(
            components
                .child_tags
                .keys()
                .any(|name| name.as_ref() == DataComponent::Damage.to_name())
        );
        assert!(
            !components
                .child_tags
                .keys()
                .any(|name| { name.as_ref() == format!("!{}", DataComponent::Damage.to_name()) })
        );

        let decoded = ItemStack::read_item_stack(&encoded).expect("stack should round-trip");
        assert_eq!(
            decoded
                .get_data_component::<DamageImpl>()
                .map(|damage| damage.damage),
            Some(7)
        );
        assert!(decoded.has_data_component(DataComponent::Damage));
        assert!(decoded.are_items_and_components_equal(&expected));
        assert_eq!(decoded.get_hash(), expected.get_hash());
    }

    #[test]
    fn duplicate_component_remove_replaces_late_value_with_tombstone() {
        let mut stack = ItemStack::new_with_component(
            1,
            &Item::IRON_SWORD,
            vec![
                (DataComponent::Damage, None),
                (
                    DataComponent::Damage,
                    Some(DamageImpl { damage: 7 }.to_dyn()),
                ),
            ],
        );
        stack.remove_data_component(DataComponent::Damage);

        let expected = ItemStack::new_with_component(
            1,
            &Item::IRON_SWORD,
            vec![(DataComponent::Damage, None)],
        );
        assert!(!stack.has_data_component(DataComponent::Damage));
        assert!(stack.get_data_component::<DamageImpl>().is_none());
        assert!(stack.are_items_and_components_equal(&expected));
        assert_eq!(stack.get_hash(), expected.get_hash());

        let mut encoded = NbtCompound::new();
        stack.write_item_stack(&mut encoded);
        let components = encoded
            .get_compound("components")
            .expect("components should be encoded");
        assert!(
            !components
                .child_tags
                .keys()
                .any(|name| name.as_ref() == DataComponent::Damage.to_name())
        );
        assert!(
            components
                .child_tags
                .keys()
                .any(|name| { name.as_ref() == format!("!{}", DataComponent::Damage.to_name()) })
        );

        let decoded = ItemStack::read_item_stack(&encoded).expect("stack should round-trip");
        assert!(!decoded.has_data_component(DataComponent::Damage));
        assert!(decoded.get_data_component::<DamageImpl>().is_none());
        assert!(decoded.are_items_and_components_equal(&expected));
        assert_eq!(decoded.get_hash(), expected.get_hash());
    }

    #[test]
    fn duplicate_component_set_updates_the_latest_value() {
        let mut stack = ItemStack::new_with_component(
            1,
            &Item::IRON_SWORD,
            vec![
                (
                    DataComponent::Damage,
                    Some(DamageImpl { damage: 3 }.to_dyn()),
                ),
                (
                    DataComponent::Damage,
                    Some(DamageImpl { damage: 5 }.to_dyn()),
                ),
            ],
        );
        stack.set_data_component(DamageImpl { damage: 9 });

        let mut expected = ItemStack::new(1, &Item::IRON_SWORD);
        expected.set_data_component(DamageImpl { damage: 9 });
        assert_eq!(stack.get_damage(), 9);
        assert!(stack.are_items_and_components_equal(&expected));
        assert_eq!(stack.get_hash(), expected.get_hash());
    }

    #[test]
    fn duplicate_lore_set_updates_the_latest_value_and_roundtrips() {
        let mut stack = ItemStack::new_with_component(
            1,
            &Item::WOODEN_AXE,
            vec![
                (
                    DataComponent::Lore,
                    Some(
                        LoreImpl {
                            lines: vec![pumpkin_util::text::TextComponent::text("old first")],
                        }
                        .to_dyn(),
                    ),
                ),
                (
                    DataComponent::Lore,
                    Some(
                        LoreImpl {
                            lines: vec![pumpkin_util::text::TextComponent::text("old latest")],
                        }
                        .to_dyn(),
                    ),
                ),
            ],
        );
        stack.set_lore(vec![pumpkin_util::text::TextComponent::text("new latest")]);

        let mut expected = ItemStack::new(1, &Item::WOODEN_AXE);
        expected.set_lore(vec![pumpkin_util::text::TextComponent::text("new latest")]);
        assert_eq!(
            stack
                .get_data_component::<LoreImpl>()
                .expect("lore should be present")
                .lines[0]
                .clone()
                .get_text(),
            "new latest"
        );
        assert!(stack.are_items_and_components_equal(&expected));
        assert_eq!(stack.get_hash(), expected.get_hash());

        let mut encoded = NbtCompound::new();
        stack.write_item_stack(&mut encoded);
        let decoded = ItemStack::read_item_stack(&encoded).expect("stack should round-trip");
        assert_eq!(
            decoded
                .get_data_component::<LoreImpl>()
                .expect("lore should round-trip")
                .lines[0]
                .clone()
                .get_text(),
            "new latest"
        );
        assert!(decoded.are_items_and_components_equal(&expected));
        assert_eq!(decoded.get_hash(), expected.get_hash());
    }

    #[test]
    fn custom_data_sets_and_reads_typed_values() {
        let mut stack = ItemStack::new(1, &Item::WOODEN_AXE);

        stack.set_custom_data("test_plugin", "marker", NbtTag::Byte(1));
        stack.set_custom_data("test_plugin", "mode", NbtTag::String("pos1".into()));
        stack.set_custom_data(
            "test_plugin",
            "payload",
            NbtTag::ByteArray(vec![0, 1, 127, -128, -1].into()),
        );

        assert_eq!(
            stack.get_custom_data("test_plugin", "marker"),
            Some(NbtTag::Byte(1))
        );
        assert_eq!(
            stack.get_custom_data("test_plugin", "mode"),
            Some(NbtTag::String("pos1".into()))
        );
        assert_eq!(
            stack.get_custom_data("test_plugin", "payload"),
            Some(NbtTag::ByteArray(vec![0, 1, 127, -128, -1].into()))
        );
        assert!(stack.has_custom_data("test_plugin", "marker"));
    }

    #[test]
    fn custom_data_sets_and_reads_full_nbt_tags() {
        let mut stack = ItemStack::new(1, &Item::WOODEN_AXE);
        let mut compound = NbtCompound::new();
        compound.child_tags.insert("byte".into(), NbtTag::Byte(1));
        compound.child_tags.insert("short".into(), NbtTag::Short(2));
        compound.child_tags.insert("int".into(), NbtTag::Int(3));
        compound.child_tags.insert("long".into(), NbtTag::Long(4));
        compound
            .child_tags
            .insert("float".into(), NbtTag::Float(5.0));
        compound
            .child_tags
            .insert("double".into(), NbtTag::Double(6.0));
        compound
            .child_tags
            .insert("string".into(), NbtTag::String("value".into()));
        compound.child_tags.insert(
            "list".into(),
            NbtTag::List(vec![NbtTag::Int(1), NbtTag::Int(2)]),
        );
        compound
            .child_tags
            .insert("byte_array".into(), NbtTag::ByteArray(vec![1, 2].into()));
        compound
            .child_tags
            .insert("int_array".into(), NbtTag::IntArray(vec![3, 4]));
        compound
            .child_tags
            .insert("long_array".into(), NbtTag::LongArray(vec![5, 6]));

        let tag = NbtTag::Compound(compound);
        stack.set_custom_data("test_plugin", "full", tag.clone());

        assert_eq!(stack.get_custom_data("test_plugin", "full"), Some(tag));
    }

    #[test]
    fn custom_data_preserves_sibling_namespaces_and_keys() {
        let mut stack = ItemStack::new(1, &Item::WOODEN_AXE);

        stack.set_custom_data("test_plugin", "marker", NbtTag::Byte(1));
        stack.set_custom_data("test_plugin", "mode", NbtTag::String("pos1".into()));
        stack.set_custom_data("other_plugin", "flag", NbtTag::Byte(1));
        stack.set_custom_data("test_plugin", "marker", NbtTag::Byte(0));

        assert_eq!(
            stack.get_custom_data("test_plugin", "marker"),
            Some(NbtTag::Byte(0))
        );
        assert_eq!(
            stack.get_custom_data("test_plugin", "mode"),
            Some(NbtTag::String("pos1".into()))
        );
        assert_eq!(
            stack.get_custom_data("other_plugin", "flag"),
            Some(NbtTag::Byte(1))
        );
    }

    #[test]
    fn remove_custom_data_removes_only_target_key_and_cleans_empty_component() {
        let mut stack = ItemStack::new(1, &Item::WOODEN_AXE);

        stack.set_custom_data("test_plugin", "marker", NbtTag::Byte(1));
        stack.set_custom_data("test_plugin", "mode", NbtTag::String("pos1".into()));
        stack.set_custom_data("other_plugin", "flag", NbtTag::Byte(1));

        stack.remove_custom_data("test_plugin", "marker");
        assert!(!stack.has_custom_data("test_plugin", "marker"));
        assert_eq!(
            stack.get_custom_data("test_plugin", "mode"),
            Some(NbtTag::String("pos1".into()))
        );
        assert_eq!(
            stack.get_custom_data("other_plugin", "flag"),
            Some(NbtTag::Byte(1))
        );
        assert!(stack.get_data_component::<CustomDataImpl>().is_some());

        stack.remove_custom_data("test_plugin", "mode");
        stack.remove_custom_data("other_plugin", "flag");
        assert!(stack.get_data_component::<CustomDataImpl>().is_none());
    }

    #[test]
    fn custom_data_preserves_other_item_components() {
        let mut stack = ItemStack::new(1, &Item::WOODEN_AXE);
        stack.patch.push((
            DataComponent::CustomName,
            Some(
                CustomNameImpl {
                    name: pumpkin_util::text::TextComponent::text("Marked Item"),
                }
                .to_dyn(),
            ),
        ));
        stack
            .patch
            .push((DataComponent::Unbreakable, Some(UnbreakableImpl.to_dyn())));

        stack.set_custom_data("test_plugin", "marker", NbtTag::Byte(1));
        stack.remove_custom_data("test_plugin", "missing");

        assert!(stack.get_data_component::<CustomNameImpl>().is_some());
        assert!(stack.get_data_component::<UnbreakableImpl>().is_some());
        assert_eq!(
            stack.get_custom_data("test_plugin", "marker"),
            Some(NbtTag::Byte(1))
        );
    }

    #[test]
    fn lore_can_be_set_and_appended() {
        let mut stack = ItemStack::new(1, &Item::WOODEN_AXE);
        stack.set_lore(vec![pumpkin_util::text::TextComponent::text("First line")]);
        stack.add_lore(pumpkin_util::text::TextComponent::text("Second line"));

        let lore = stack
            .get_data_component::<LoreImpl>()
            .expect("lore component should be present");
        assert_eq!(lore.lines.len(), 2);
        assert_eq!(lore.lines[0].clone().get_text(), "First line");
        assert_eq!(lore.lines[1].clone().get_text(), "Second line");
    }

    #[test]
    fn custom_data_survives_item_stack_nbt_roundtrip() {
        let mut stack = ItemStack::new(1, &Item::WOODEN_AXE);
        stack.set_custom_data("test_plugin", "marker", NbtTag::Byte(1));
        stack.set_custom_data("test_plugin", "mode", NbtTag::String("pos1".into()));
        stack
            .patch
            .push((DataComponent::Unbreakable, Some(UnbreakableImpl.to_dyn())));

        let mut compound = NbtCompound::new();
        stack.write_item_stack(&mut compound);
        let decoded = ItemStack::read_item_stack(&compound).expect("stack should decode");

        assert_eq!(
            decoded.get_custom_data("test_plugin", "marker"),
            Some(NbtTag::Byte(1))
        );
        assert_eq!(
            decoded.get_custom_data("test_plugin", "mode"),
            Some(NbtTag::String("pos1".into()))
        );
        assert!(decoded.get_data_component::<UnbreakableImpl>().is_some());
    }

    #[test]
    fn use_remainder_nbt_preserves_template_and_custom_components() {
        let mut custom_data = NbtCompound::new();
        custom_data.put_int("x", 7);
        let mut components = NbtCompound::new();
        components.put("minecraft:custom_data", NbtTag::Compound(custom_data));

        let mut template = NbtCompound::new();
        template.put_string("id", "minecraft:bowl".to_owned());
        template.put_int("count", 2);
        template.put_compound("components", components.clone());

        let remainder = UseRemainderImpl::read_data(&NbtTag::Compound(template))
            .expect("official remainder template should decode");
        assert_eq!(remainder.convert_into.item, &Item::BOWL);
        assert_eq!(remainder.convert_into.item_count, 2);
        assert_eq!(
            remainder
                .convert_into
                .get_data_component::<CustomDataImpl>()
                .expect("custom data should survive")
                .data
                .get_int("x"),
            Some(7)
        );

        let mut encoded = NbtCompound::new();
        remainder.convert_into.write_item_stack(&mut encoded);
        assert_eq!(encoded.get_compound("components"), Some(&components));
    }

    #[test]
    fn use_remainder_accepts_official_bare_item_template() {
        let remainder = UseRemainderImpl::read_data(&NbtTag::String("minecraft:bowl".into()))
            .expect("bare item template should decode");
        assert_eq!(remainder.convert_into.item, &Item::BOWL);
        assert_eq!(remainder.convert_into.item_count, 1);
        assert!(remainder.convert_into.patch.is_empty());
    }

    #[test]
    fn item_stack_count_uses_java_number_narrowing_before_range() {
        for tag in [
            NbtTag::Byte(2),
            NbtTag::Short(2),
            NbtTag::Long(2),
            NbtTag::Float(2.9),
            NbtTag::Double(2.9),
        ] {
            let mut stack = NbtCompound::new();
            stack.put_string("id", "minecraft:bucket".to_owned());
            stack.put("count", tag);
            assert_eq!(ItemStack::read_item_stack(&stack).unwrap().item_count, 2);
        }
        for tag in [NbtTag::String("2".into()), NbtTag::List(Vec::new())] {
            let mut stack = NbtCompound::new();
            stack.put_string("id", "minecraft:bucket".to_owned());
            stack.put("count", tag);
            assert!(ItemStack::read_item_stack(&stack).is_none());
        }
    }

    #[test]
    fn item_stack_nbt_uses_official_defaults_and_rejects_invalid_templates() {
        let mut missing_count = NbtCompound::new();
        missing_count.put_string("id", "minecraft:bucket".to_owned());
        let decoded = ItemStack::read_item_stack(&missing_count).expect("count defaults to one");
        assert_eq!(decoded.item_count, 1);
        assert!(decoded.patch.is_empty());

        let mut wrong_shape = NbtCompound::new();
        wrong_shape.put_string("id", "minecraft:bucket".to_owned());
        wrong_shape.put_string("count", "one".to_owned());
        assert!(ItemStack::read_item_stack(&wrong_shape).is_none());

        let mut wrong_components = NbtCompound::new();
        wrong_components.put_string("id", "minecraft:bucket".to_owned());
        wrong_components.put_string("components", "not a compound".to_owned());
        assert!(ItemStack::read_item_stack(&wrong_components).is_none());

        for count in [0, -1, 100] {
            let mut invalid = NbtCompound::new();
            invalid.put_string("id", "minecraft:bucket".to_owned());
            invalid.put_int("count", count);
            assert!(
                ItemStack::read_item_stack(&invalid).is_none(),
                "count={count}"
            );
        }

        for id in ["minecraft:air", "minecraft:not_an_item"] {
            let mut invalid = NbtCompound::new();
            invalid.put_string("id", id.to_owned());
            assert!(ItemStack::read_item_stack(&invalid).is_none(), "id={id}");
        }
    }

    #[test]
    fn generated_use_remainder_defaults_keep_official_templates() {
        for (item, remainder, count) in [
            (&Item::MILK_BUCKET, &Item::BUCKET, 1),
            (&Item::HONEY_BOTTLE, &Item::GLASS_BOTTLE, 1),
            (&Item::MUSHROOM_STEW, &Item::BOWL, 1),
            (&Item::BEETROOT_SOUP, &Item::BOWL, 1),
        ] {
            let component = item
                .components
                .iter()
                .find(|(id, _)| *id == DataComponent::UseRemainder)
                .map(|(_, component)| get::<UseRemainderImpl>(*component))
                .expect("generated use_remainder component");
            assert_eq!(component.convert_into.item, remainder);
            assert_eq!(component.convert_into.item_count, count);
            assert!(component.convert_into.patch.is_empty());
        }
    }

    #[test]
    fn nested_item_stack_components_are_reflexive_and_hash_consistent() {
        let nested = ItemStack::new(1, &Item::BOWL);
        let mut first = ItemStack::new(1, &Item::CROSSBOW);
        first.patch.push((
            DataComponent::Container,
            Some(Box::new(ContainerImpl {
                items: vec![(0, nested.clone())],
            })),
        ));
        first.patch.push((
            DataComponent::BundleContents,
            Some(Box::new(BundleContentsImpl {
                items: vec![nested.clone()],
            })),
        ));
        let second = first.clone();
        assert!(first.are_equal(&first));
        assert!(first.are_equal(&second));
        assert_eq!(first.get_hash(), second.get_hash());
        assert_eq!(
            UseRemainderImpl {
                convert_into: first.clone(),
            },
            UseRemainderImpl {
                convert_into: second,
            }
        );
    }

    #[test]
    fn translated_item_name_survives_item_stack_nbt_roundtrip() {
        let mut stack = ItemStack::new(1, &Item::FILLED_MAP);
        stack.patch.push((
            DataComponent::ItemName,
            Some(
                ItemNameImpl {
                    name: crate::data_component_impl::ItemName::translated("filled_map.mansion"),
                }
                .to_dyn(),
            ),
        ));

        let mut compound = NbtCompound::new();
        stack.write_item_stack(&mut compound);
        let decoded = ItemStack::read_item_stack(&compound).expect("stack should decode");

        assert_eq!(
            decoded
                .get_data_component::<ItemNameImpl>()
                .expect("item name should decode")
                .name,
            crate::data_component_impl::ItemName::Component(
                pumpkin_util::text::TextComponent::translate("filled_map.mansion", vec![]),
            )
        );
    }

    // ── damage_item ───────────────────────────────────────────────

    #[test]
    fn damage_zero_amount_is_noop() {
        let mut stack = iron_sword();
        assert_eq!(stack.damage_item(0), DamageResult::Untouched);
        assert_eq!(stack.get_damage(), 0);
    }

    #[test]
    fn damage_negative_amount_is_noop() {
        let cases: &[i32] = &[-1, -5, -10, -100];
        for &amount in cases {
            let mut stack = iron_sword();
            assert_eq!(
                stack.damage_item(amount),
                DamageResult::Untouched,
                "expected no damage for amount={amount}"
            );
            assert_eq!(stack.get_damage(), 0, "damage mismatch for amount={amount}");
        }
    }

    #[test]
    fn damage_non_damageable_item_is_noop() {
        // AIR has no MaxDamage component.
        let mut stack = ItemStack::new(1, &Item::AIR);
        assert_eq!(stack.damage_item(1), DamageResult::Untouched);
    }

    #[test]
    fn damage_unbreakable_item_is_noop() {
        let cases: &[i32] = &[1, 5, 10, 100, 250];
        for &amount in cases {
            let mut stack = iron_sword();
            stack
                .patch
                .push((DataComponent::Unbreakable, Some(UnbreakableImpl.to_dyn())));
            assert_eq!(
                stack.damage_item(amount),
                DamageResult::Untouched,
                "expected no damage for unbreakable item, amount={amount}"
            );
            assert_eq!(
                stack.get_damage(),
                0,
                "damage mismatch for unbreakable item, amount={amount}"
            );
        }
    }

    #[test]
    fn damage_increases_damage_value() {
        // Without Unbreaking, every point of damage is applied.
        // Each sub-array is (amount, expected_damage); each case gets a fresh iron_sword.
        let cases: &[(i32, i32)] = &[(1, 1), (5, 5), (10, 10), (100, 100), (249, 249)];
        for &(amount, expected) in cases {
            let mut stack = iron_sword();
            assert_eq!(
                stack.damage_item(amount),
                DamageResult::Damaged,
                "expected damage_item to return Damaged for amount={amount}"
            );
            assert_eq!(
                stack.get_damage(),
                expected,
                "damage mismatch for amount={amount}"
            );
        }
    }

    #[test]
    fn damage_accumulates() {
        // Each entry: (first_amount, second_amount, expected_total)
        let cases: &[(i32, i32, i32)] = &[(100, 50, 150), (10, 20, 30), (1, 1, 2), (50, 100, 150)];
        for &(first, second, expected) in cases {
            let mut stack = iron_sword();
            let _ = stack.damage_item(first);
            let _ = stack.damage_item(second);
            assert_eq!(
                stack.get_damage(),
                expected,
                "accumulated damage mismatch for first={first}, second={second}"
            );
        }
    }

    #[test]
    fn damage_breaks_item_when_exceeding_max() {
        // Iron Sword max_damage = 250; any amount >= 250 should destroy it.
        let cases: &[i32] = &[250, 260, 300, 1000];
        for &amount in cases {
            let mut stack = iron_sword();
            assert_eq!(
                stack.damage_item(amount),
                DamageResult::Broken,
                "expected item to break for amount={amount}"
            );
            assert!(
                stack.is_empty(),
                "item should be destroyed for amount={amount}"
            );
        }
    }

    #[test]
    fn damage_item_changes_component_equality() {
        let mut stack = iron_sword();
        let original = stack.clone();
        assert_eq!(stack.damage_item(1), DamageResult::Damaged);
        assert!(
            !stack.are_equal(&original),
            "durability patch must differ so inventory sync sends SET_SLOT"
        );
        assert_eq!(stack.get_damage(), 1);
    }

    #[test]
    fn damage_breaks_single_item_to_empty() {
        let mut stack = iron_sword();
        let _ = stack.damage_item(300);
        assert!(stack.is_empty());
        assert_eq!(stack.item_count, 0);
    }

    // ── repair_item ──────────────────────────────────────────────────

    #[test]
    fn repair_zero_amount_is_noop() {
        let initial_damages: &[i32] = &[1, 5, 10, 100, 249];
        for &initial in initial_damages {
            let mut stack = iron_sword();
            stack.set_damage(initial);
            assert_eq!(
                stack.repair_item(0),
                0,
                "repair(0) should return 0 for initial={initial}"
            );
            assert_eq!(
                stack.get_damage(),
                initial,
                "damage should be unchanged for initial={initial}"
            );
        }
    }

    #[test]
    fn repair_negative_amount_is_noop() {
        let cases: &[i32] = &[-1, -5, -10, -100];
        for &amount in cases {
            let mut stack = iron_sword();
            stack.set_damage(10);
            assert_eq!(
                stack.repair_item(amount),
                0,
                "repair({amount}) should return 0"
            );
            assert_eq!(
                stack.get_damage(),
                10,
                "damage should be unchanged for repair({amount})"
            );
        }
    }

    #[test]
    fn repair_undamaged_item_is_noop() {
        let amounts: &[i32] = &[1, 5, 10, 100];
        for &amount in amounts {
            let mut stack = iron_sword();
            assert_eq!(
                stack.repair_item(amount),
                0,
                "repair({amount}) on undamaged item should return 0"
            );
            assert_eq!(
                stack.get_damage(),
                0,
                "undamaged item should remain at 0 after repair({amount})"
            );
        }
    }

    #[test]
    fn repair_partial() {
        // Each entry: (initial_damage, repair_amount, expected_repaired, expected_remaining)
        let cases: &[(i32, i32, i32, i32)] = &[
            (20, 8, 8, 12),
            (50, 25, 25, 25),
            (100, 30, 30, 70),
            (249, 1, 1, 248),
        ];
        for &(initial, repair, exp_repaired, exp_remaining) in cases {
            let mut stack = iron_sword();
            stack.set_damage(initial);
            let repaired = stack.repair_item(repair);
            assert_eq!(
                repaired, exp_repaired,
                "repaired amount mismatch for initial={initial}, repair={repair}"
            );
            assert_eq!(
                stack.get_damage(),
                exp_remaining,
                "remaining damage mismatch for initial={initial}, repair={repair}"
            );
        }
    }

    #[test]
    fn repair_capped_at_current_damage() {
        // Each entry: (initial_damage, repair_amount); repair exceeds damage, so repaired == initial.
        let cases: &[(i32, i32)] = &[(5, 6), (5, 100), (10, 11), (100, 200)];
        for &(initial, repair) in cases {
            let mut stack = iron_sword();
            stack.set_damage(initial);
            let repaired = stack.repair_item(repair);
            assert_eq!(
                repaired, initial,
                "repaired amount mismatch for initial={initial}, repair={repair}"
            );
            assert_eq!(
                stack.get_damage(),
                0,
                "damage should be 0 after over-repair for initial={initial}"
            );
        }
    }

    #[test]
    fn repair_fully_clears_damage_component() {
        let mut stack = iron_sword();
        stack.set_damage(10);
        stack.repair_item(10);
        assert_eq!(stack.get_damage(), 0);
        // set_damage(0) removes the Damage patch entry.
        assert!(
            !stack
                .patch
                .iter()
                .any(|(id, _)| *id == DataComponent::Damage)
        );
    }

    // ── stacked item breaking ────────────────────────────────────────

    #[test]
    fn damage_stacked_item_breaks_one_and_resets_durability() {
        // Two Iron Swords (max_damage 250) at damage 249 — one hit away from breaking.
        // Without Unbreaking the damage roll is always applied, so this is deterministic.
        let mut stack = ItemStack::new(2, &Item::IRON_SWORD);
        stack.set_damage(249);

        let result = stack.damage_item(1);

        assert_eq!(
            result,
            DamageResult::Broken,
            "stacked item at max damage should return Broken"
        );
        assert_eq!(stack.item_count, 1, "stack count should drop from 2 to 1");
        assert_eq!(
            stack.get_damage(),
            0,
            "remaining sword's durability should reset to 0 after breaking"
        );
        assert!(
            !stack.is_empty(),
            "one sword should still remain in the stack"
        );
    }

    // ── weapon category predicates ───────────────────────────────────

    /// 2-durability combat weapons (axes/pickaxes/shovels/hoes) must match their category predicate.
    #[test]
    fn weapon_categories_identify_2_cost_items() {
        // Items that should have is_axe / is_pickaxe / is_shovel / is_hoe = true.
        let axes: &[&Item] = &[
            &Item::WOODEN_AXE,
            &Item::STONE_AXE,
            &Item::IRON_AXE,
            &Item::GOLDEN_AXE,
            &Item::DIAMOND_AXE,
            &Item::NETHERITE_AXE,
        ];
        let pickaxes: &[&Item] = &[
            &Item::WOODEN_PICKAXE,
            &Item::STONE_PICKAXE,
            &Item::IRON_PICKAXE,
            &Item::GOLDEN_PICKAXE,
            &Item::DIAMOND_PICKAXE,
            &Item::NETHERITE_PICKAXE,
        ];
        let shovels: &[&Item] = &[
            &Item::WOODEN_SHOVEL,
            &Item::STONE_SHOVEL,
            &Item::IRON_SHOVEL,
            &Item::GOLDEN_SHOVEL,
            &Item::DIAMOND_SHOVEL,
            &Item::NETHERITE_SHOVEL,
        ];
        let hoes: &[&Item] = &[
            &Item::WOODEN_HOE,
            &Item::STONE_HOE,
            &Item::IRON_HOE,
            &Item::GOLDEN_HOE,
            &Item::DIAMOND_HOE,
            &Item::NETHERITE_HOE,
        ];

        for item in axes {
            let stack = ItemStack::new(1, item);
            assert!(stack.is_axe(), "{} should be an axe", item.registry_key);
            assert!(
                !stack.is_sword(),
                "{} should not be a sword",
                item.registry_key
            );
        }
        for item in pickaxes {
            let stack = ItemStack::new(1, item);
            assert!(
                stack.is_pickaxe(),
                "{} should be a pickaxe",
                item.registry_key
            );
        }
        for item in shovels {
            let stack = ItemStack::new(1, item);
            assert!(
                stack.is_shovel(),
                "{} should be a shovel",
                item.registry_key
            );
        }
        for item in hoes {
            let stack = ItemStack::new(1, item);
            assert!(stack.is_hoe(), "{} should be a hoe", item.registry_key);
        }

        // Swords should cost 1, so they must NOT match any 2-cost predicate.
        let swords: &[&Item] = &[
            &Item::IRON_SWORD,
            &Item::DIAMOND_SWORD,
            &Item::NETHERITE_SWORD,
        ];
        for item in swords {
            let stack = ItemStack::new(1, item);
            assert!(stack.is_sword(), "{} should be a sword", item.registry_key);
            assert!(
                !stack.is_axe(),
                "{} should not be an axe",
                item.registry_key
            );
            assert!(
                !stack.is_pickaxe(),
                "{} should not be a pickaxe",
                item.registry_key
            );
        }
    }

    // ── Unbreaking (statistical) ─────────────────────────────────────

    /// Helper: iron sword with Unbreaking at `level`.
    fn with_unbreaking(item: &'static Item, level: i32) -> ItemStack {
        let mut s = ItemStack::new(1, item);
        s.patch.push((
            DataComponent::Enchantments,
            Some(
                EnchantmentsImpl {
                    enchantment: std::borrow::Cow::Owned(vec![(&Enchantment::UNBREAKING, level)]),
                }
                .to_dyn(),
            ),
        ));
        s
    }

    /// Unbreaking III tool: 25% apply probability. 4 000 trials, expect ~1 000 hits (window 865–1135).
    /// ±5σ confidence window ensures regressions are caught; CI-safe and statistically meaningful.
    /// Note: uses thread-local rand::random().
    /// Could be made fully deterministic by refactoring should_apply_durability_damage_with to accept RNG parameter.
    #[test]
    fn unbreaking_iii_tool_applies_roughly_25_percent_of_hits() {
        let mut stack = with_unbreaking(&Item::NETHERITE_PICKAXE, 3);
        let mut applied: u32 = 0;
        for _ in 0..4_000 {
            if stack.damage_item(1) != DamageResult::Untouched {
                applied += 1;
            }
        }
        assert!(
            (865..=1_135).contains(&applied),
            "Unbreaking III tool: expected ~1 000 applications in 4 000 trials, got {applied}"
        );
    }

    /// Unbreaking III armor: 70% apply probability. 500 trials, expect ~350 hits (window 300–400).
    /// See tool test notes on thread-local RNG; refactor would allow full determinism via seeded RNG parameter.
    #[test]
    fn unbreaking_iii_armor_applies_roughly_70_percent_of_hits() {
        let mut stack = with_unbreaking(&Item::DIAMOND_CHESTPLATE, 3);
        let mut applied: u32 = 0;
        for _ in 0..500 {
            if stack.damage_item(1) != DamageResult::Untouched {
                applied += 1;
            }
        }
        // ~350 expected with 70% probability, ±5σ confidence (300–400 window, ~99.7% non-flaky).
        assert!(
            (300..=400).contains(&applied),
            "Unbreaking III armor: expected ~350 applications in 500 trials, got {applied}"
        );
    }

    // ── set_damage ───────────────────────────────────────────────────

    #[test]
    fn set_damage_negative_clamps_to_zero() {
        let cases: &[i32] = &[-1, -10, -100, i32::MIN];
        for &amount in cases {
            let mut stack = iron_sword();
            stack.set_damage(amount);
            assert_eq!(
                stack.get_damage(),
                0,
                "damage should clamp to 0 for set_damage({amount})"
            );
        }
    }
}
