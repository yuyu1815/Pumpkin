use std::sync::{Arc, atomic::Ordering};

use tracing::{debug, info, warn};

use crate::{
    entity::{Entity, EntityBase, player::Player},
    net::ClientPlatform,
    plugin::player::{
        player_change_world::PlayerChangeWorldEvent, player_leave::PlayerLeaveEvent,
        player_respawn::PlayerRespawnEvent,
    },
    world::{PlayerRemovalReason, World},
};
use pumpkin_data::{dimension::Dimension, translation};
use pumpkin_inventory::Clearable;
use pumpkin_protocol::{
    bedrock::{
        client::{
            add_player::CAddPlayer,
            common::BuildPlatform,
            player_list::{CPlayerList, PlayerListEntry, Skin},
            remove_actor::CRemoveActor,
            set_actor_data::PropertySyncData,
        },
        network_item::NetworkItemStackDescriptor,
    },
    codec::{var_int::VarInt, var_long::VarLong, var_ulong::VarULong},
    java::client::play::{
        CGameEvent, CPlayerSpawnPosition, CRemoveEntities, CRemovePlayerInfo, CRespawn, GameEvent,
        PlayerSpawnData,
    },
};
use pumpkin_util::{
    math::{position::BlockPos, vector2::Vector2, vector3::Vector3},
    resource_location::ResourceLocation,
    text::{TextComponent, color::NamedColor},
};
use pumpkin_world::biome;

impl World {
    pub(crate) fn despawn_dead_java_player_for_bedrock(&self, subject: &Entity) {
        let Some(player) = self.get_player_by_id(subject.entity_id) else {
            return;
        };
        if matches!(player.client.as_ref(), ClientPlatform::Java(_)) {
            self.broadcast_to_chunk_bedrock(
                subject.chunk_pos.load(),
                &CRemoveActor::new(VarLong(subject.entity_id.into())),
            );
        }
    }

    async fn refresh_java_player_for_bedrock(&self, subject: &Player) {
        if !matches!(subject.client.as_ref(), ClientPlatform::Java(_)) {
            return;
        }

        let entity = subject.get_entity();
        let entity_id = subject.entity_id();
        let position = entity.pos.load();
        let velocity = entity.velocity.load();
        let player_list = CPlayerList {
            action: CPlayerList::ACTION_ADD,
            entries: vec![PlayerListEntry {
                uuid: subject.gameprofile.id,
                entity_unique_id: VarLong(entity_id.into()),
                username: subject.gameprofile.name.clone(),
                xuid: String::new(),
                platform_chat_id: String::new(),
                build_platform: BuildPlatform::Unknown,
                skin: (**subject.bedrock_skin.load()).clone(),
                is_teacher: false,
                is_host: false,
                is_sub_client: false,
                player_color: [0; 4],
            }],
        };
        let add_player = CAddPlayer {
            uuid: subject.gameprofile.id,
            player_name: subject.gameprofile.name.clone(),
            target_runtime_id: VarULong(entity_id as u64),
            platform_chat_id: String::new(),
            position: Vector3::new(position.x as f32, position.y as f32, position.z as f32),
            velocity: Vector3::new(velocity.x as f32, velocity.y as f32, velocity.z as f32),
            rotation: Vector2::new(entity.pitch.load(), entity.yaw.load()),
            y_head_rotation: entity.head_yaw.load(),
            carried_item: NetworkItemStackDescriptor::default(),
            player_game_type: subject.gamemode.load().into(),
            entity_data: entity.bedrock_metadata(),
            synced_properties: PropertySyncData::default(),
            abilities_data: pumpkin_protocol::bedrock::client::SerializedAbilitiesData {
                target_player_raw_id: entity_id as i64,
                player_permissions:
                    pumpkin_protocol::bedrock::client::PlayerPermissionLevel::Visitor,
                command_permissions: pumpkin_protocol::bedrock::client::CommandPermissionLevel::Any,
                layers: vec![
                    pumpkin_protocol::bedrock::client::SerializedAbilitiesDataSerializedLayer {
                        serialized_layer: 0,
                        abilities_set: 0,
                        ability_value: 0,
                        fly_speed: 0.05,
                        vertical_fly_speed: 0.05,
                        walk_speed: 0.1,
                    },
                ],
            },
            actor_links: Vec::new(),
            device_id: String::new(),
            build_platform: BuildPlatform::Unknown,
        };
        let remove = CRemoveActor::new(VarLong(entity_id.into()));

        for recipient in self.players.load().iter() {
            if let ClientPlatform::Bedrock(client) = recipient.client.as_ref() {
                client.send_packet(&remove).await;
                client.send_packet(&player_list).await;
                client.send_packet(&add_player).await;
            }
        }
    }
    #[allow(clippy::too_many_lines)]
    pub async fn respawn_player(self: &Arc<Self>, player: &Arc<Player>, alive: bool) {
        let last_pos = player.get_entity().last_pos.load();
        let death_dimension = ResourceLocation::from(player.world().dimension.minecraft_name);
        let death_location = BlockPos(Vector3::new(
            last_pos.x.round() as i32,
            last_pos.y.round() as i32,
            last_pos.z.round() as i32,
        ));

        let data_kept = u8::from(alive);

        let server = self.server.upgrade();
        let default_world = server.as_ref().map_or_else(
            || self.clone(),
            |s| s.get_world_from_dimension(&Dimension::OVERWORLD),
        );

        // Copy spawn info from default world level_info to avoid holding lock across await
        let (spawn_x, spawn_y, spawn_z, spawn_yaw, spawn_pitch, keep_inventory) = {
            let info = default_world.level_info.load();
            (
                info.spawn_x,
                info.spawn_y,
                info.spawn_z,
                info.spawn_yaw,
                info.spawn_pitch,
                info.game_rules.keep_inventory,
            )
        };

        // Get respawn position and dimension
        let (position, yaw, pitch, respawn_dimension) = if let Some(respawn) =
            player.calculate_respawn_point().await
        {
            (
                respawn.position,
                respawn.yaw,
                respawn.pitch,
                respawn.dimension,
            )
        } else {
            // No valid respawn point - send notification if player had one set
            if player
                .respawn_point
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
            {
                player
                    .send_client_packet(&CGameEvent::new(GameEvent::NoRespawnBlockAvailable, 0.0))
                    .await;
                let mut guard = player
                    .respawn_point
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(point) = guard.as_ref()
                    && !point.force
                {
                    *guard = None;
                }
            }

            // FIXME: This spawn position calculation is incorrect. Should use vanilla's
            // proper spawn position calculation (see #1381). The y-level calculation
            // needs to account for spawn radius and find a safe spawn position.
            let chunk_pos = Vector2::new(spawn_x >> 4, spawn_z >> 4);
            default_world
                .level
                .get_or_fetch_chunk(chunk_pos, |_| ())
                .await;
            let top = default_world.get_top_block(Vector2::new(spawn_x, spawn_z));
            let pos_y = if top > default_world.dimension.min_y {
                top + 1
            } else {
                spawn_y
            };

            (
                Vector3::new(
                    f64::from(spawn_x) + 0.5,
                    f64::from(pos_y),
                    f64::from(spawn_z) + 0.5,
                ),
                spawn_yaw,
                spawn_pitch,
                default_world.dimension.clone(),
            )
        };

        let mut spawn_loc_event = crate::plugin::api::events::player::player_spawn_location::PlayerSpawnLocationEvent::new(
            player.clone(),
            position,
        );
        if let Some(ref s) = server {
            s.plugin_manager.fire(s, &mut spawn_loc_event).await;
        }
        let position = spawn_loc_event.spawn_pos;

        // Candidate destination world for a cross-dimension respawn.
        let candidate_world = if respawn_dimension == self.dimension {
            None
        } else {
            server.as_ref().map_or_else(
                || {
                    warn!("Could not get server for cross-dimension respawn");
                    None
                },
                |s| {
                    let worlds = s.worlds.load();
                    worlds
                        .iter()
                        .find(|w| w.dimension == respawn_dimension)
                        .cloned()
                },
            )
        };

        // Fire PlayerChangeWorldEvent (cancellable) before the transfer; it runs before
        // the non-cancellable PlayerRespawnEvent, which observes the resolved world.
        let (resolved_world, position, yaw, pitch) = if let Some(new_world) = candidate_world {
            if let Some(ref s) = server {
                let mut event = PlayerChangeWorldEvent {
                    player: player.clone(),
                    previous_world: self.clone(),
                    new_world: new_world.clone(),
                    position,
                    yaw,
                    pitch,
                    cancelled: false,
                };
                s.plugin_manager.fire(s, &mut event).await;

                if event.cancelled {
                    (None, position, yaw, pitch)
                } else {
                    let destination = event.new_world;
                    let position = event.position;
                    let yaw = event.yaw;
                    let pitch = event.pitch;

                    // Skip the transfer if redirected back to the current world.
                    if destination.uuid != self.uuid {
                        debug!(
                            "Cross-dimension respawn: {} -> {}",
                            self.dimension.minecraft_name, destination.dimension.minecraft_name
                        );

                        // Detach from the old world before publishing into the new one, so no
                        // observer sees the player in a world whose chunk manager doesn't match.
                        self.remove_player(player, PlayerRemovalReason::DimensionTransfer)
                            .await;
                        player.unload_watched_chunks(self).await;
                        player.change_world_chunks(&self.level, &destination);
                        player.living_entity.entity.set_world(destination.clone());
                        destination.publish_player_membership(player);
                    }

                    (Some(destination), position, yaw, pitch)
                }
            } else {
                warn!("Server dropped during cross-dimension respawn");
                (None, position, yaw, pitch)
            }
        } else {
            if respawn_dimension != self.dimension {
                warn!(
                    "Target world {:?} not found, using world spawn in {:?}",
                    respawn_dimension, self.dimension
                );
            }
            (None, position, yaw, pitch)
        };

        // Cancelled or unresolved cross-dimension respawns fall back to the current
        // world's spawn below; otherwise the resolved values from the event apply.
        let (target_world, position, yaw, pitch) = resolved_world.as_ref().map_or_else(
            || (self.clone(), position, yaw, pitch),
            |new_world| (new_world.clone(), position, yaw, pitch),
        );

        // Notify plugins that the player has respawned (non-cancellable).
        if let Some(server) = self.server.upgrade() {
            server
                .plugin_manager
                .fire(
                    &server,
                    &mut PlayerRespawnEvent::new(
                        player.clone(),
                        self.clone(),
                        target_world.clone(),
                        position,
                        yaw,
                        pitch,
                        alive,
                    ),
                )
                .await;
        }

        // Send respawn packet with target dimension (using send_packet_now to ensure proper order)
        player
            .send_client_packet(&CRespawn::new(
                PlayerSpawnData::new(
                    target_world.dimension.clone(),
                    biome::hash_seed(target_world.level.seed.0),
                    player.gamemode.load() as u8,
                    player.gamemode.load() as i8,
                    false,
                    false,
                    Some((death_dimension, death_location)),
                    VarInt(player.get_entity().portal_cooldown.load(Ordering::Relaxed) as i32),
                    target_world.sea_level.into(),
                ),
                data_kept,
            ))
            .await;

        // Inform the client of the default spawn position so the client doesn't
        // fall back to (0, 2, 0) while the world reloads (fixes rubberbanding).
        // This must be sent after the CRespawn packet for proper client positioning.
        let spawn_block_pos = BlockPos(Vector3::new(
            position.x.round() as i32,
            position.y.round() as i32,
            position.z.round() as i32,
        ));
        let bedrock_dimension = match target_world.dimension.minecraft_name {
            "minecraft:the_nether" => 1,
            "minecraft:the_end" => 2,
            _ => 0,
        };
        player
            .send_packet_now_editioned(
                &CPlayerSpawnPosition::new(
                    spawn_block_pos,
                    yaw,
                    pitch,
                    target_world.dimension.minecraft_name.to_string(),
                ),
                &pumpkin_protocol::bedrock::client::CSetSpawnPosition {
                    spawn_position_type:
                        pumpkin_protocol::bedrock::client::SpawnPositionType::WorldRespawn,
                    block_position: spawn_block_pos,
                    dimension_type: bedrock_dimension.into(),
                    spawn_block_pos,
                },
            )
            .await;

        player.living_entity.reset_state();

        player.send_permission_lvl_update();

        player.hunger_manager.restart();

        if !keep_inventory {
            player.set_experience(0, 0.0, 0);
            player.inventory.clear();
        }

        // Set entity position BEFORE loading chunks, so chunks load at the right location
        // This mirrors the initial spawn flow where update_position is called before teleport
        player.get_entity().set_pos(position);
        player.get_entity().set_rotation(yaw, pitch);
        player.get_entity().last_pos.store(position);

        // TODO: difficulty, exp bar, status effect

        // Load chunks and send world info FIRST (before teleport packet)
        target_world.send_world_info(player, position, yaw, pitch);

        // Ensure at least the center chunk is sent synchronously before teleport.
        if let crate::net::ClientPlatform::Java(java_client) = player.client.as_ref() {
            let center_chunk = player.get_entity().chunk_pos.load();
            let chunk = target_world
                .level
                .get_or_fetch_chunk(center_chunk, std::clone::Clone::clone)
                .await;
            java_client.send_chunks(&[chunk]).await;
        }

        // Send teleport packet after at least the center chunk was delivered
        player.request_teleport(position, yaw, pitch);

        target_world.refresh_java_player_for_bedrock(player).await;
    }
    /// Adds a player to the world and broadcasts a join message if enabled.
    ///
    /// This function takes a player's UUID and an `Arc<Player>` reference.
    /// It inserts the player into the world's `current_players` map using the UUID as the key.
    /// Additionally, it broadcasts a join message to all connected players in the world.
    ///
    /// # Arguments
    ///
    /// * `player`: An `Arc<Player>` reference to the player object.
    pub(crate) fn publish_player_membership(&self, player: &Arc<Player>) {
        self.players.publish(player);
    }

    pub fn add_player(&self, player: &Arc<Player>) -> Result<(), String> {
        self.publish_player_membership(player);
        self.entity_tracker
            .add_entity(&(player.clone() as Arc<dyn EntityBase>), self);
        Ok(())
    }

    /// Must only be called after the player's own `CLogin` packet has been sent.
    pub fn pair_new_player_with_tracked_entities(&self, player: &Arc<Player>) {
        self.entity_tracker
            .pair_new_player_with_tracked_entities(player, self);
    }

    /// Removes a player from the world and broadcasts a disconnect message if enabled.
    ///
    /// This function removes a player from the world based on their `Player` reference.
    /// It performs the following actions:
    ///
    /// 1. Removes the player from the `current_players` map using their UUID.
    /// 2. Broadcasts a `CRemovePlayerInfo` packet to all connected players to inform them about the player leaving.
    /// 3. Removes the player's entity from the world using its entity ID.
    /// 4. Optionally sends a disconnect message to all other players notifying them about the player leaving.
    ///
    /// # Arguments
    ///
    /// * `player`: A reference to the `Player` object to be removed.
    /// * `reason`: Whether this is a disconnect or a cross-dimension transfer.
    ///
    /// # Notes
    ///
    /// - This function assumes `broadcast_packet_expect` and `remove_entity` are defined elsewhere.
    /// - The disconnect message sending is currently optional. Consider making it a configurable option.
    pub async fn remove_player(
        &self,
        player: &Arc<Player>,
        reason: PlayerRemovalReason,
    ) -> Option<Arc<Player>> {
        let removed_player = self.players.remove(player);
        if let Some(ref player) = removed_player {
            let uuid = player.gameprofile.id;
            let entity_id = player.entity_id();
            let replacement_is_present = self
                .players
                .load()
                .iter()
                .any(|candidate| candidate.gameprofile.id == uuid)
                || self.server.upgrade().is_some_and(|server| {
                    server.worlds.load().iter().any(|world| {
                        world
                            .players
                            .load()
                            .iter()
                            .any(|candidate| candidate.gameprofile.id == uuid)
                    })
                });

            if matches!(reason, PlayerRemovalReason::Disconnect) {
                let _chat_lifecycle = player
                    .chat_lifecycle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Retire before taking the state-store lock. Any delayed
                // session update/verify using this Player then fails without a
                // historical UUID tombstone.
                player.chat_owner.retire();
                let session_id = player
                    .chat_session
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .session_id;
                crate::net::chat::state::clear_inbound_state(uuid, session_id, &player.chat_owner);
            }

            if replacement_is_present {
                self.entity_tracker.remove_entity_preserving_player(
                    player.as_ref() as &dyn EntityBase,
                    self,
                    uuid,
                );
            } else {
                self.entity_tracker
                    .remove_entity(player.as_ref() as &dyn EntityBase, self);
            }

            let bedrock_remove_player = CPlayerList {
                action: CPlayerList::ACTION_REMOVE,
                entries: vec![PlayerListEntry {
                    uuid,
                    entity_unique_id: VarLong(entity_id as i64),
                    username: player.gameprofile.name.clone(),
                    xuid: String::new(),
                    platform_chat_id: String::new(),
                    build_platform: BuildPlatform::Unknown,
                    skin: Skin::steve(),
                    is_teacher: false,
                    is_host: false,
                    is_sub_client: false,
                    player_color: [0, 0, 0, 0],
                }],
            };

            if !replacement_is_present {
                self.broadcast_editioned(&CRemovePlayerInfo::new(&[uuid]), &bedrock_remove_player);
            }

            self.broadcast_editioned(
                &CRemoveEntities::new(&[entity_id.into()]),
                &CRemoveActor::new(VarLong(entity_id as i64)),
            );

            if matches!(reason, PlayerRemovalReason::Disconnect) {
                let msg_comp = TextComponent::translate_cross(
                    translation::java::MULTIPLAYER_PLAYER_LEFT,
                    translation::bedrock::MULTIPLAYER_PLAYER_LEFT,
                    [TextComponent::text(player.gameprofile.name.clone())],
                )
                .color_named(NamedColor::Yellow);
                let mut event = PlayerLeaveEvent::new(player.clone(), msg_comp);

                if let Some(server) = self.server.upgrade() {
                    server.plugin_manager.fire(&server, &mut event).await;

                    if !event.cancelled {
                        for player in self.players.load().iter() {
                            player.send_system_message(&event.leave_message);
                        }
                        info!("{}", event.leave_message.to_pretty_console());
                    }
                }
            }
        }
        removed_player
    }
}
