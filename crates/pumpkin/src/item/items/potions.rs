use std::sync::Arc;

use crate::entity::Entity;
use crate::entity::EntityBase;
use crate::entity::player::Player;
use crate::entity::projectile::{
    lingering_potion::LingeringPotionEntity, splash_potion::SplashPotionEntity,
};
use crate::item::{ItemBehaviour, ItemMetadata};
use pumpkin_data::entity::EntityType;
use pumpkin_data::item::Item;
use pumpkin_data::sound::Sound;
use pumpkin_util::Hand;

pub struct PotionItem;
pub struct SplashPotionItem;
pub struct LingeringPotionItem;

impl ItemMetadata for PotionItem {
    fn ids() -> Box<[u16]> {
        [Item::POTION.id].into()
    }
}

impl ItemMetadata for SplashPotionItem {
    fn ids() -> Box<[u16]> {
        [Item::SPLASH_POTION.id].into()
    }
}

impl ItemMetadata for LingeringPotionItem {
    fn ids() -> Box<[u16]> {
        [Item::LINGERING_POTION.id].into()
    }
}

const POWER: f32 = 0.5;

impl ItemBehaviour for PotionItem {
    fn normal_use(&self, _item: &Item, _player: &Player) {
        // Drinking is handled by the consumable flow in the server (active hand + consumption tick).
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl ItemBehaviour for SplashPotionItem {
    fn normal_use_with_hand(
        &self,
        _item: &Item,
        player: &Player,
        hand: Hand,
        _yaw: f32,
        _pitch: f32,
    ) {
        let position = player.position();
        let world = player.world();
        world.play_sound(
            Sound::EntityWitchThrow,
            pumpkin_data::sound::SoundCategory::Neutral,
            &position,
        );
        let entity = Entity::new(world.clone(), position, &EntityType::SPLASH_POTION);
        let splash = SplashPotionEntity::new_shot(entity, player.get_entity());

        splash.set_item_stack(player.inventory.get_stack_in_hand(hand));

        let (yaw, pitch) = player.rotation();
        splash.thrown.set_velocity_from(pitch, yaw, 0.0, POWER, 1.0);

        world.spawn_entity(Arc::new(splash));

        let mut stack = player.inventory.get_stack_in_hand(hand);
        stack.decrement_unless_creative(player.gamemode.load(), 1);
        player.inventory.set_stack_in_hand(hand, stack);
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl ItemBehaviour for LingeringPotionItem {
    fn normal_use_with_hand(
        &self,
        _item: &Item,
        player: &Player,
        hand: Hand,
        _yaw: f32,
        _pitch: f32,
    ) {
        let position = player.position();
        let world = player.world();
        world.play_sound(
            Sound::EntityWitchThrow,
            pumpkin_data::sound::SoundCategory::Neutral,
            &position,
        );
        let entity = Entity::new(world.clone(), position, &EntityType::LINGERING_POTION);
        let ling = LingeringPotionEntity::new_shot(entity, player.get_entity());

        ling.set_item_stack(player.inventory.get_stack_in_hand(hand));

        let (yaw, pitch) = player.rotation();
        ling.thrown.set_velocity_from(pitch, yaw, 0.0, POWER, 1.0);

        world.spawn_entity(Arc::new(ling));

        let mut stack = player.inventory.get_stack_in_hand(hand);
        stack.decrement_unless_creative(player.gamemode.load(), 1);
        player.inventory.set_stack_in_hand(hand, stack);
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
