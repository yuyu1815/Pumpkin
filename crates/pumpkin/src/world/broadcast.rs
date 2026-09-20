// Extracted from world/mod.rs; integration will declare this module and remove the originals.
use std::{collections::BTreeMap, sync::Arc};

use bytes::BufMut;
use rand::{RngExt, rng};
use tracing::error;
use uuid::Uuid;

use super::World;
use crate::{
    entity::{Entity, EntityBase, player::Player},
    net::{ClientPlatform, bedrock::BedrockClient, java::JavaClient},
    world::chunker::is_within_chebyshev_distance,
};
use pumpkin_data::{
    effect::StatusEffect,
    entity::EntityStatus,
    particle::Particle,
    sound::{Sound, SoundCategory},
    world::{RAW, WorldEvent},
};
use pumpkin_protocol::codec::data_component::data_to_proto_sound;
use pumpkin_protocol::{BClientPacket, ClientPacket, IdOr};
use pumpkin_protocol::{
    bedrock::{
        client::level_sound_event::CLevelSoundEvent,
        server::{
            actor_event::{ActorEventID, SActorEvent},
            text::SText,
        },
    },
    codec::{var_int::VarInt, var_ulong::VarULong},
    java::{
        client::play::{
            CDamageEvent, CDisguisedChatMessage, CEntityStatus, CParticle, CRemoveMobEffect,
            CSetBlockDestroyStage, CSetEntityMetadata, CSoundEffect, CSystemChatMessage,
            CUpdateMobEffect, CWorldEvent, Metadata,
        },
        server::play::SChatMessage,
    },
};
use pumpkin_util::{
    math::{position::BlockPos, vector2::Vector2, vector3::Vector3},
    text::TextComponent,
    version::JavaMinecraftVersion,
};

impl World {
    pub fn broadcast_entity_event(
        &self,
        entity: &Entity,
        java_status: EntityStatus,
        bedrock_status: Option<ActorEventID>,
    ) {
        let je_packet = CEntityStatus::new(entity.entity_id, java_status as i8);
        if let Some(be_event) = bedrock_status {
            let be_packet = SActorEvent {
                target_runtime_id: VarULong(entity.entity_id as u64),
                event_id: be_event,
                data: VarInt(0),
                fire_at_position: None,
            };
            self.send_to_tracking_players_and_self_editioned(entity, &je_packet, &be_packet);
        } else {
            self.send_to_tracking_players_and_self(entity, &je_packet);
        }
    }

    pub fn broadcast_damage_event(
        &self,
        entity: &Entity,
        damage_type_id: i32,
        source_entity_id: Option<i32>,
        cause_entity_id: Option<i32>,
        position: Option<Vector3<f64>>,
    ) {
        let je_packet = CDamageEvent::new(
            entity.entity_id.into(),
            damage_type_id.into(),
            source_entity_id.map(Into::into),
            cause_entity_id.map(Into::into),
            position,
        );
        self.send_to_tracking_players_and_self(entity, &je_packet);
    }

    pub fn send_entity_status(
        &self,
        entity: &Entity,
        java_status: EntityStatus,
        bedrock_status: Option<ActorEventID>,
    ) {
        self.broadcast_entity_event(entity, java_status, bedrock_status);
    }

    pub fn send_remove_mob_effect(&self, entity: &Entity, effect_type: &'static StatusEffect) {
        let je_packet =
            CRemoveMobEffect::new(entity.entity_id.into(), VarInt(i32::from(effect_type.id)));

        let be_packet = pumpkin_protocol::bedrock::client::CMobEffect {
            target_runtime_id: VarULong(entity.entity_id as u64),
            event_id: pumpkin_protocol::bedrock::client::CMobEffect::EVENT_REMOVE,
            effect_id: VarInt(effect_type.to_bedrock_id()),
            effect_amplifier: VarInt(0),
            show_particles: false,
            effect_duration_ticks: VarInt(0),
            tick: VarULong(0),
            ambient: false,
        };
        self.send_to_tracking_players_and_self_editioned(entity, &je_packet, &be_packet);
    }

    pub fn send_add_mob_effect(&self, entity: &Entity, effect: &pumpkin_data::potion::Effect) {
        let mut flags: i8 = 0;
        if effect.ambient {
            flags |= 0x01;
        }
        if effect.show_particles {
            flags |= 0x02;
        }
        if effect.show_icon {
            flags |= 0x04;
        }

        let je_packet = CUpdateMobEffect::new(
            VarInt(entity.entity_id),
            VarInt(i32::from(effect.effect_type.id)),
            VarInt(i32::from(effect.amplifier)),
            VarInt(effect.duration),
            flags,
        );

        let be_packet = pumpkin_protocol::bedrock::client::CMobEffect {
            target_runtime_id: VarULong(entity.entity_id as u64),
            event_id: pumpkin_protocol::bedrock::client::CMobEffect::EVENT_ADD,
            effect_id: VarInt(effect.effect_type.to_bedrock_id()),
            effect_amplifier: VarInt(i32::from(effect.amplifier)),
            show_particles: effect.show_particles,
            effect_duration_ticks: VarInt(effect.duration),
            tick: VarULong(0),
            ambient: effect.ambient,
        };

        self.send_to_tracking_players_and_self_editioned(entity, &je_packet, &be_packet);
    }

    pub fn send_to_tracking_players<P: ClientPacket + Sync>(&self, entity: &Entity, packet: &P) {
        if let Some(tracked) = self.entity_tracker.get_tracked_entity(entity.entity_id) {
            tracked.send_to_tracking_players(packet, self);
        }
    }

    pub fn send_to_tracking_players_bedrock<P: BClientPacket + Sync>(
        &self,
        entity: &Entity,
        packet: &P,
    ) {
        if let Some(tracked) = self.entity_tracker.get_tracked_entity(entity.entity_id) {
            tracked.send_to_tracking_players_bedrock(packet, self);
        }
    }

    pub fn send_to_tracking_players_editioned<J: ClientPacket + Sync, B: BClientPacket + Sync>(
        &self,
        entity: &Entity,
        je_packet: &J,
        be_packet: &B,
    ) {
        if let Some(tracked) = self.entity_tracker.get_tracked_entity(entity.entity_id) {
            tracked.send_to_tracking_players_editioned(je_packet, be_packet, self);
        }
    }

    pub fn send_to_tracking_players_and_self<P: ClientPacket + Sync>(
        &self,
        entity: &Entity,
        packet: &P,
    ) {
        if let Some(tracked) = self.entity_tracker.get_tracked_entity(entity.entity_id) {
            tracked.send_to_tracking_players_and_self(packet, self);
        }
    }

    pub fn send_to_tracking_players_and_self_editioned<
        J: ClientPacket + Sync,
        B: BClientPacket + Sync,
    >(
        &self,
        entity: &Entity,
        je_packet: &J,
        be_packet: &B,
    ) {
        if let Some(tracked) = self.entity_tracker.get_tracked_entity(entity.entity_id) {
            tracked.send_to_tracking_players_and_self_editioned(je_packet, be_packet, self);
        }
    }

    pub fn send_to_tracking_players_filtered<P: ClientPacket + Sync, F: Fn(&Player) -> bool>(
        &self,
        entity: &Entity,
        packet: &P,
        filter: F,
    ) {
        if let Some(tracked) = self.entity_tracker.get_tracked_entity(entity.entity_id) {
            tracked.send_to_tracking_players_filtered(packet, self, filter);
        }
    }

    pub fn send_to_tracking_players_filtered_editioned<
        J: ClientPacket + Sync,
        B: BClientPacket + Sync,
        F: Fn(&Player) -> bool,
    >(
        &self,
        entity: &Entity,
        je_packet: &J,
        be_packet: &B,
        filter: F,
    ) {
        if let Some(tracked) = self.entity_tracker.get_tracked_entity(entity.entity_id) {
            tracked.send_to_tracking_players_filtered_editioned(je_packet, be_packet, self, filter);
        }
    }

    pub fn is_tracked_by_any_player(&self, entity: &Entity) -> bool {
        self.entity_tracker
            .is_tracked_by_any_player(entity.entity_id)
    }

    pub(crate) fn collect_java_recipients_by_version<'a>(
        players: impl Iterator<Item = &'a Arc<Player>>,
    ) -> BTreeMap<JavaMinecraftVersion, Vec<&'a JavaClient>> {
        let mut recipients_by_version: BTreeMap<JavaMinecraftVersion, Vec<&'a JavaClient>> =
            BTreeMap::new();
        for player in players {
            if let ClientPlatform::Java(java_client) = player.client.as_ref() {
                recipients_by_version
                    .entry(java_client.version.load())
                    .or_default()
                    .push(java_client);
            }
        }
        recipients_by_version
    }

    pub fn broadcast_java_clients<'a, P: ClientPacket>(
        packet: &P,
        recipients: impl Iterator<Item = &'a JavaClient>,
    ) {
        let mut recipients_by_version: BTreeMap<JavaMinecraftVersion, Vec<&JavaClient>> =
            BTreeMap::new();
        for client in recipients {
            recipients_by_version
                .entry(client.version.load())
                .or_default()
                .push(client);
        }
        Self::broadcast_java_grouped(packet, recipients_by_version);
    }

    pub(super) fn broadcast_java_grouped<P: ClientPacket>(
        packet: &P,
        recipients_by_version: BTreeMap<JavaMinecraftVersion, Vec<&JavaClient>>,
    ) {
        for (version, recipients) in recipients_by_version {
            let packet_data = match JavaClient::serialize_packet_for_version(packet, version) {
                Ok(packet_data) => packet_data,
                Err(pumpkin_protocol::ser::WritingError::UnsupportedVersion(_)) => {
                    continue;
                }
                Err(err) => {
                    error!(
                        "Failed to serialize packet {} for version {:?}: {}",
                        std::any::type_name::<P>(),
                        version,
                        err
                    );
                    continue;
                }
            };

            for recipient in recipients {
                recipient.try_enqueue_packet(packet_data.clone());
            }
        }
    }

    pub(super) fn broadcast_bedrock_grouped<'a, P: BClientPacket>(
        packet: &P,
        recipients: impl Iterator<Item = &'a Arc<BedrockClient>>,
    ) {
        for recipient in recipients {
            match recipient.serialize_packet(packet) {
                Ok(packet_data) => recipient.try_enqueue_packet(packet_data),
                Err(err) => {
                    error!(
                        "Failed to serialize bedrock packet {}: {}",
                        std::any::type_name::<P>(),
                        err
                    );
                }
            }
        }
    }

    pub fn broadcast_packet_all<P: ClientPacket>(&self, packet: &P) {
        let players = self.players.load();
        let recipients_by_version = Self::collect_java_recipients_by_version(players.iter());
        Self::broadcast_java_grouped(packet, recipients_by_version);
    }

    pub fn broadcast_system_message(&self, message: &TextComponent, overlay: bool) {
        let je_packet = CSystemChatMessage::new(message, overlay);
        let be_packet = Self::component_to_bedrock_text(message);
        self.broadcast_editioned(&je_packet, &be_packet);
    }

    fn component_to_bedrock_text(message: &TextComponent) -> SText<'static> {
        match &*message.0.content {
            pumpkin_util::text::TextContent::Translate {
                translate,
                bedrock_translate,
                with,
            } => {
                let key = bedrock_translate.as_deref().unwrap_or(translate.as_ref());
                let parameters = with
                    .iter()
                    .map(pumpkin_util::text::TextComponentBase::to_bedrock_string)
                    .collect();
                SText::translation(key.to_string(), parameters)
            }
            _ => SText::system_message(
                message
                    .0
                    .to_bedrock_legacy(pumpkin_util::translation::Locale::EnUs),
            ),
        }
    }

    pub fn broadcast_message(
        &self,
        message: &TextComponent,
        sender_name: &TextComponent,
        chat_type: u8,
        target_name: Option<&TextComponent>,
    ) {
        let be_packet = SText::new(message.clone().get_text(), sender_name.clone().get_text());
        let je_packet =
            CDisguisedChatMessage::new(message, (chat_type + 1).into(), sender_name, target_name);

        self.broadcast_editioned(&je_packet, &be_packet);
    }

    pub fn broadcast_editioned<J: ClientPacket, B: BClientPacket>(
        &self,
        je_packet: &J,
        be_packet: &B,
    ) {
        let players = self.players.load();
        let je_recipients_by_version = Self::collect_java_recipients_by_version(players.iter());

        Self::broadcast_java_grouped(je_packet, je_recipients_by_version);
        Self::broadcast_bedrock_grouped(
            be_packet,
            players.iter().filter_map(|p| match p.client.as_ref() {
                ClientPlatform::Bedrock(be) => Some(be),
                ClientPlatform::Java(_) => None,
            }),
        );
    }

    pub fn broadcast_chat_message(
        &self,
        message: &crate::net::chat::PlayerChatMessage,
        is_filtered: impl Fn(&Player) -> bool,
        sender_player: Option<&Arc<Player>>,
        chat_type: VarInt,
        sender_name: &TextComponent,
        target_name: Option<&TextComponent>,
    ) {
        let tracked = crate::net::chat::OutgoingChatMessage::create(message.clone());
        let mut was_fully_filtered = false;

        let players = self.players.load();
        for player in players.iter() {
            let filtered = is_filtered(player);
            tracked.send_to_player(player, filtered, chat_type, sender_name, target_name);
            was_fully_filtered |= filtered && message.is_fully_filtered();
        }

        if was_fully_filtered && let Some(sender) = sender_player {
            let filter_notice =
                TextComponent::translate(pumpkin_data::translation::java::CHAT_FILTERED_FULL, [])
                    .color_named(pumpkin_util::text::color::NamedColor::Red)
                    .italic();
            sender.send_system_message(&filter_notice);
        }
    }

    pub fn broadcast_secure_player_chat(
        &self,
        sender: &Arc<Player>,
        chat_message: &SChatMessage<'_>,
        decorated_message: &TextComponent,
    ) {
        let messages_sent: i32 = sender
            .chat_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .messages_sent;
        let sender_last_seen = {
            let cache = sender
                .signature_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.last_seen.as_ref().to_vec()
        };

        let link = crate::net::chat::SignedMessageLink::new(
            messages_sent,
            sender.gameprofile.id,
            Uuid::nil(),
        );
        let signed_body = crate::net::chat::SignedMessageBody::new(
            chat_message.message.to_string(),
            chat_message.timestamp,
            chat_message.salt,
            sender_last_seen,
        );
        let player_chat_msg = crate::net::chat::PlayerChatMessage::new(
            link,
            chat_message.signature.map(std::convert::Into::into),
            signed_body,
            Some(decorated_message.clone()),
            crate::net::chat::FilterMask::PassThrough,
        );

        self.broadcast_chat_message(
            &player_chat_msg,
            Player::is_text_filtering_enabled,
            Some(sender),
            (RAW + 1).into(),
            &TextComponent::empty(),
            None,
        );

        sender
            .chat_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .messages_sent += 1;
    }

    pub fn broadcast_packet_except_editioned<J: ClientPacket, B: BClientPacket>(
        &self,
        except: &[uuid::Uuid],
        je_packet: &J,
        be_packet: &B,
    ) {
        let players = self.players.load();
        let mut java_recipients = Vec::new();
        let mut bedrock_recipients = Vec::new();

        for p in players.iter() {
            if except.contains(&p.gameprofile.id) {
                continue;
            }
            match p.client.as_ref() {
                ClientPlatform::Java(_) => java_recipients.push(p),
                ClientPlatform::Bedrock(be_client) => bedrock_recipients.push(be_client),
            }
        }

        let recipients_by_version =
            Self::collect_java_recipients_by_version(java_recipients.into_iter());
        Self::broadcast_java_grouped(je_packet, recipients_by_version);
        Self::broadcast_bedrock_grouped(be_packet, bedrock_recipients.into_iter());
    }

    pub(super) fn broadcast_skin_parts<B: BClientPacket>(
        &self,
        except: &[uuid::Uuid],
        entity_id: i32,
        skin_parts: u8,
        be_packet: &B,
    ) {
        let players = self.players.load();
        let mut java_recipients = Vec::new();
        let mut bedrock_recipients = Vec::new();

        for p in players.iter() {
            if except.contains(&p.gameprofile.id) {
                continue;
            }
            match p.client.as_ref() {
                ClientPlatform::Java(_) => java_recipients.push(p),
                ClientPlatform::Bedrock(be_client) => bedrock_recipients.push(be_client),
            }
        }

        let recipients_by_version =
            Self::collect_java_recipients_by_version(java_recipients.into_iter());

        for (version, recipients) in recipients_by_version {
            if version < JavaMinecraftVersion::V_1_21 {
                continue;
            }
            let mut buf = Vec::new();
            for meta in [
                Metadata::new(
                    pumpkin_data::tracked_data::player::PLAYER_MODE_CUSTOMISATION,
                    skin_parts,
                ),
                Metadata::new(
                    pumpkin_data::tracked_data::player::PLAYER_MODE_CUSTOMIZATION_ID,
                    skin_parts,
                ),
            ] {
                let _ = meta.write(&mut buf, &version);
            }
            buf.put_u8(255);
            let packet = CSetEntityMetadata::new(entity_id.into(), buf.into());
            if let Ok(packet_data) = JavaClient::serialize_packet_for_version(&packet, version) {
                for recipient in recipients {
                    recipient.try_enqueue_packet(packet_data.clone());
                }
            }
        }

        Self::broadcast_bedrock_grouped(be_packet, bedrock_recipients.into_iter());
    }

    pub fn broadcast_packet_except<P: ClientPacket>(&self, except: &[uuid::Uuid], packet: &P) {
        let players = self.players.load();
        let recipients_by_version = Self::collect_java_recipients_by_version(
            players
                .iter()
                .filter(|candidate| !except.contains(&candidate.gameprofile.id)),
        );
        Self::broadcast_java_grouped(packet, recipients_by_version);
    }

    pub fn spawn_particle(
        &self,
        position: Vector3<f64>,
        offset: Vector3<f32>,
        max_speed: f32,
        particle_count: i32,
        particle: Particle,
    ) {
        for player in self.players.load().iter() {
            player.spawn_particle(position, offset, max_speed, particle_count, particle);
        }
    }

    pub fn play_sound(&self, sound: Sound, category: SoundCategory, position: &Vector3<f64>) {
        self.play_sound_raw(sound as u16, category, position, 1.0, 1.0);
    }

    pub fn play_sound_event(
        &self,
        sound: &pumpkin_data::data_component_impl::IdOr<
            pumpkin_data::data_component_impl::SoundEvent,
        >,
        category: SoundCategory,
        position: &Vector3<f64>,
    ) {
        let seed = rng().random::<i64>();
        let packet = CSoundEffect::new(
            data_to_proto_sound(sound),
            category,
            position,
            1.0,
            1.0,
            seed,
        );
        self.broadcast_packet_all(&packet);
    }

    pub fn play_sound_event_expect(
        &self,
        player: &Player,
        sound: &pumpkin_data::data_component_impl::IdOr<
            pumpkin_data::data_component_impl::SoundEvent,
        >,
        category: SoundCategory,
        position: &Vector3<f64>,
    ) {
        let seed = rng().random::<i64>();
        let packet = CSoundEffect::new(
            data_to_proto_sound(sound),
            category,
            position,
            1.0,
            1.0,
            seed,
        );
        self.broadcast_packet_except(&[player.gameprofile.id], &packet);
    }

    pub fn play_sound_fine(
        &self,
        sound: Sound,
        category: SoundCategory,
        position: &Vector3<f64>,
        volume: f32,
        pitch: f32,
    ) {
        self.play_sound_raw(sound as u16, category, position, volume, pitch);
    }

    pub fn play_custom_sound(
        &self,
        sound_name: &str,
        category: SoundCategory,
        position: &Vector3<f64>,
        volume: f32,
        pitch: f32,
    ) {
        let seed = rand::random::<i64>();
        let packet = CSoundEffect::new(
            pumpkin_protocol::IdOr::Value(pumpkin_protocol::SoundEvent {
                sound_name: sound_name.into(),
                range: None,
            }),
            category,
            position,
            volume,
            pitch,
            seed,
        );
        self.broadcast_packet_all(&packet);
    }

    pub fn spawn_particles(
        &self,
        particle: pumpkin_data::particle::Particle,
        pos: Vector3<f64>,
        count: u32,
        offset: Vector3<f32>,
        max_speed: f32,
    ) {
        let packet = CParticle::new(
            false,
            false,
            pos,
            offset,
            max_speed,
            count as i32,
            (particle.to_id() as i32).into(),
            &[],
        );
        self.broadcast_packet_all(&packet);
    }

    pub fn play_bedrock_level_sound(
        &self,
        sound_id: &str,
        position: &Vector3<f64>,
        extra_data: i32,
    ) {
        let packet = CLevelSoundEvent {
            sound_event: sound_id.to_string(),
            position: Vector3::new(position.x as f32, position.y as f32, position.z as f32),
            data: VarInt(extra_data),
            actor_identifier: String::new(),
            is_baby: false,
            is_global: false,
            actor_unique_id: 0,
            fire_at_position: None,
        };
        let chunk_pos = BlockPos::floored_v(*position).chunk_position();

        for player in self.players.load().iter() {
            if is_within_chebyshev_distance(chunk_pos, player.get_entity().chunk_pos.load(), 1)
                && let ClientPlatform::Bedrock(client) = player.client.as_ref()
                && let Ok(data) = client.serialize_packet(&packet)
            {
                client.try_enqueue_packet(data);
            }
        }
    }

    pub fn play_sound_expect(
        &self,
        player: &Player,
        sound: Sound,
        category: SoundCategory,
        position: &Vector3<f64>,
    ) {
        self.play_sound_raw_expect(player, sound as u16, category, position, 1.0, 1.0);
    }

    pub fn play_sound_raw(
        &self,
        sound_id: u16,
        category: SoundCategory,
        position: &Vector3<f64>,
        volume: f32,
        pitch: f32,
    ) {
        let seed = rand::rng().random::<i64>();
        let packet = CSoundEffect::new(IdOr::Id(sound_id), category, position, volume, pitch, seed);

        // Calculate the number of chunks the sound can be heard from based on its volume.
        let audible_chunks = f64::from(volume.max(1.0)).ceil() as i32;
        let chunk_pos = BlockPos::floored_v(*position).chunk_position();

        let players = self.players.load();
        let recipients = players.iter().filter(|p| {
            let center = p.get_entity().chunk_pos.load();
            // If the sound reaches their chunk, send it!
            is_within_chebyshev_distance(chunk_pos, center, audible_chunks)
        });

        let recipients_by_version = Self::collect_java_recipients_by_version(recipients);
        Self::broadcast_java_grouped(&packet, recipients_by_version);
    }

    pub fn play_sound_raw_expect(
        &self,
        player: &Player,
        sound_id: u16,
        category: SoundCategory,
        position: &Vector3<f64>,
        volume: f32,
        pitch: f32,
    ) {
        let seed = rand::rng().random::<i64>();
        let packet = CSoundEffect::new(IdOr::Id(sound_id), category, position, volume, pitch, seed);

        let audible_chunks = f64::from(volume.max(1.0)).ceil() as i32;
        let chunk_pos = BlockPos::floored_v(*position).chunk_position();

        let players = self.players.load();
        let recipients = players.iter().filter(|p| {
            // Skip the expected player
            if p.gameprofile.id == player.gameprofile.id {
                return false;
            }

            let center = p.get_entity().chunk_pos.load();
            is_within_chebyshev_distance(chunk_pos, center, audible_chunks)
        });

        let recipients_by_version = Self::collect_java_recipients_by_version(recipients);
        Self::broadcast_java_grouped(&packet, recipients_by_version);
    }

    pub fn play_block_sound(&self, sound: Sound, category: SoundCategory, position: BlockPos) {
        let new_vec = Vector3::new(
            f64::from(position.0.x) + 0.5,
            f64::from(position.0.y) + 0.5,
            f64::from(position.0.z) + 0.5,
        );
        self.play_sound(sound, category, &new_vec);
    }

    pub fn play_block_sound_expect(
        &self,
        player: &Player,
        sound: Sound,
        category: SoundCategory,
        position: BlockPos,
    ) {
        let new_vec = Vector3::new(
            f64::from(position.0.x) + 0.5,
            f64::from(position.0.y) + 0.5,
            f64::from(position.0.z) + 0.5,
        );
        self.play_sound_expect(player, sound, category, &new_vec);
    }

    pub fn sync_world_event(&self, world_event: WorldEvent, position: BlockPos, data: i32) {
        let chunk_pos = position.chunk_position();
        self.broadcast_to_chunk(
            chunk_pos,
            &CWorldEvent::new(world_event as i32, position, data, false),
        );
    }

    pub fn sync_global_world_event(&self, world_event: WorldEvent, position: BlockPos, data: i32) {
        self.broadcast_packet_all(&CWorldEvent::new(world_event as i32, position, data, true));
    }

    pub fn set_block_destroy_stage(&self, entity_id: i32, location: BlockPos, stage: i8) {
        let chunk_pos = location.chunk_position();
        let packet = CSetBlockDestroyStage::new(entity_id.into(), location, stage);
        self.broadcast_to_chunk(chunk_pos, &packet);
    }

    pub fn broadcast_to_chunk<P: ClientPacket>(&self, chunk_pos: Vector2<i32>, packet: &P) {
        let players = self.players.load();

        let recipients = players.iter().filter(|p| {
            p.watched_section
                .load()
                .is_within_distance(chunk_pos.x, chunk_pos.y)
        });

        let recipients_by_version = Self::collect_java_recipients_by_version(recipients);
        Self::broadcast_java_grouped(packet, recipients_by_version);
    }

    pub fn broadcast_to_chunk_bedrock<P: BClientPacket>(
        &self,
        chunk_pos: Vector2<i32>,
        packet: &P,
    ) {
        let players = self.players.load();
        let recipients = players.iter().filter_map(|player| {
            if player
                .watched_section
                .load()
                .is_within_distance(chunk_pos.x, chunk_pos.y)
                && let ClientPlatform::Bedrock(client) = player.client.as_ref()
            {
                return Some(client);
            }
            None
        });
        Self::broadcast_bedrock_grouped(packet, recipients);
    }

    pub fn broadcast_to_chunk_editioned<J: ClientPacket, B: BClientPacket>(
        &self,
        chunk_pos: Vector2<i32>,
        je_packet: &J,
        be_packet: &B,
    ) {
        let players = self.players.load();
        let mut java_recipients = Vec::new();
        let mut bedrock_recipients = Vec::new();

        let recipients = players.iter().filter(|p| {
            p.watched_section
                .load()
                .is_within_distance(chunk_pos.x, chunk_pos.y)
        });

        for p in recipients {
            match p.client.as_ref() {
                ClientPlatform::Java(_) => java_recipients.push(p),
                ClientPlatform::Bedrock(be_client) => bedrock_recipients.push(be_client),
            }
        }

        let recipients_by_version =
            Self::collect_java_recipients_by_version(java_recipients.into_iter());
        Self::broadcast_java_grouped(je_packet, recipients_by_version);
        Self::broadcast_bedrock_grouped(be_packet, bedrock_recipients.into_iter());
    }

    pub fn broadcast_to_chunk_except<P: ClientPacket>(
        &self,
        chunk_pos: Vector2<i32>,
        except: &[uuid::Uuid],
        packet: &P,
    ) {
        let players = self.players.load();

        let recipients = players.iter().filter(|p| {
            if except.contains(&p.get_entity().entity_uuid) {
                return false;
            }
            p.watched_section
                .load()
                .is_within_distance(chunk_pos.x, chunk_pos.y)
        });

        let recipients_by_version = Self::collect_java_recipients_by_version(recipients);
        Self::broadcast_java_grouped(packet, recipients_by_version);
    }

    pub fn broadcast_to_chunk_except_editioned<J: ClientPacket, B: BClientPacket>(
        &self,
        chunk_pos: Vector2<i32>,
        except: &[uuid::Uuid],
        je_packet: &J,
        be_packet: &B,
    ) {
        let players = self.players.load();
        let recipients = players.iter().filter(|p| {
            if except.contains(&p.get_entity().entity_uuid) {
                return false;
            }
            p.watched_section
                .load()
                .is_within_distance(chunk_pos.x, chunk_pos.y)
        });

        let mut java_recipients = Vec::new();
        let mut bedrock_recipients = Vec::new();

        for p in recipients {
            match p.client.as_ref() {
                ClientPlatform::Java(_) => java_recipients.push(p),
                ClientPlatform::Bedrock(be_client) => bedrock_recipients.push(be_client),
            }
        }

        let je_recipients_by_version =
            Self::collect_java_recipients_by_version(java_recipients.into_iter());
        Self::broadcast_java_grouped(je_packet, je_recipients_by_version);
        Self::broadcast_bedrock_grouped(be_packet, bedrock_recipients.into_iter());
    }
}
