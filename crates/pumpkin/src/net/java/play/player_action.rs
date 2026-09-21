#[allow(clippy::wildcard_imports)]
use super::*;

impl JavaClient {
    #[expect(clippy::too_many_lines)]
    pub fn handle_player_action(
        &self,
        player: &Arc<Player>,
        player_action: &SPlayerAction,
        server: &Server,
    ) {
        if !player.has_client_loaded() {
            return;
        }
        player.update_last_action_time();
        match Status::try_from(player_action.status.0) {
            Ok(status) => match status {
                Status::StartedDigging => {
                    if !player.can_interact_with_block_at(&player_action.position, 1.0) {
                        warn!(
                            "Player {0} tried to interact with block out of reach at {1}",
                            player.gameprofile.name, player_action.position
                        );
                        self.update_sequence(player_action.sequence.0);
                        return;
                    }
                    let position = player_action.position;
                    player.stop_mining_if_target_changed(position);
                    let entity = &player.get_entity();
                    let world = entity.world.load_full();
                    let (block, state) = world.get_block_and_state(&position);

                    // Vanilla rejects mutation when mayBuild is false before firing block-damage
                    // hooks. Adventure can_break predicates are not yet evaluated by the item
                    // component owner, so do not let this path become an unrestricted bypass.
                    if position.0.y > world.get_top_y() || !player.may_build() {
                        player.stop_mining();
                        self.sync_block_state_to_client(&world, position);
                        self.update_sequence(player_action.sequence.0);
                        return;
                    }

                    if let Some(server_arc) = world.server.upgrade() {
                        let mut event =
                            crate::plugin::api::events::block::block_damage::BlockDamageEvent::new(
                                player.clone(),
                                block,
                                position,
                                false,
                            );
                        server_arc
                            .plugin_manager
                            .fire_blocking(&server_arc, &mut event);
                        if event.cancelled {
                            self.update_sequence(player_action.sequence.0);
                            return;
                        }
                    }

                    if block == &pumpkin_data::Block::NOTE_BLOCK {
                        let props =
                            pumpkin_data::block_properties::NoteBlockLikeProperties::from_state_id(
                                state.id,
                            );
                        crate::block::blocks::note::NoteBlock::play_note(&props, &world, &position);
                        player.increment_stat(
                            StatisticCategory::Custom,
                            CustomStatistic::PlayNoteblock as i32,
                            1,
                        );
                    }

                    let inventory = player.inventory();
                    let held = inventory.held_item();
                    if !server.item_registry.can_mine(held.item, player) {
                        player.try_send_client_packet(&CBlockUpdate::new(
                            position,
                            VarInt(i32::from(state.id.as_u16())),
                        ));
                        self.update_sequence(player_action.sequence.0);
                        return;
                    }

                    // TODO: do validation
                    // TODO: Config
                    if player.gamemode.load() == GameMode::Creative {
                        // Creative START is an immediate replacement for any prior destroy state.
                        player.stop_mining();
                        // Block break & play sound
                        let new_state = world.break_block(
                            &position,
                            Some(player),
                            BlockFlags::NOTIFY_ALL | BlockFlags::SKIP_DROPS,
                        );
                        if new_state.is_some() {
                            server
                                .block_registry
                                .broken(&world, block, player, &position, server, state);
                        }
                        self.sync_block_state_to_client(&world, position);
                        self.update_sequence(player_action.sequence.0);
                        return;
                    }
                    player.start_mining_time.store(
                        player.tick_counter.load(Ordering::Relaxed),
                        Ordering::Relaxed,
                    );
                    if !state.is_air() {
                        let speed = block::calc_block_breaking(player, state, block);
                        // Instant break
                        if speed.is_finite() && speed >= 1.0 {
                            player.stop_mining();
                            let broken_state = world.get_block_state(&position);
                            let can_harvest = player.can_harvest(broken_state, block);
                            let flags = if can_harvest {
                                BlockFlags::NOTIFY_ALL
                            } else {
                                BlockFlags::SKIP_DROPS | BlockFlags::NOTIFY_ALL
                            };
                            let new_state = world.break_block(&position, Some(player), flags);
                            if new_state.is_some() {
                                server.block_registry.broken(
                                    &world,
                                    block,
                                    player,
                                    &position,
                                    server,
                                    broken_state,
                                );
                                player.apply_tool_damage_for_block_break(broken_state);
                                if can_harvest {
                                    player.add_exhaustion(MINE_BLOCK_EXHAUSTION);
                                }
                                let item_id = player.inventory().held_item().item.id;
                                player.increment_stat(StatisticCategory::Used, item_id as i32, 1);
                                player.increment_stat(
                                    StatisticCategory::Mined,
                                    block.id.as_u16() as i32,
                                    1,
                                );
                            }
                            self.sync_block_state_to_client(&world, position);
                        } else {
                            player.mining.store(true, Ordering::Relaxed);
                            *player
                                .mining_pos
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = position;
                            let progress = (speed * 10.0) as i32;
                            player
                                .current_block_breaking_speed
                                .store(speed.to_bits(), Ordering::Relaxed);
                            world.set_block_breaking(
                                entity,
                                position,
                                BlockBreakingProgress::Start {
                                    stage: progress,
                                    speed,
                                },
                            );
                            player
                                .current_block_destroy_stage
                                .store(progress, Ordering::Relaxed);
                        }
                    }
                    self.update_sequence(player_action.sequence.0);
                }
                Status::CancelledDigging => {
                    if !player.can_interact_with_block_at(&player_action.position, 1.0) {
                        warn!(
                            "Player {0} tried to interact with block out of reach at {1}",
                            player.gameprofile.name, player_action.position
                        );
                        self.update_sequence(player_action.sequence.0);
                        return;
                    }
                    let world = player.world();
                    if player_action.position.0.y > world.get_top_y() {
                        player.stop_mining();
                        self.sync_block_state_to_client(&world, player_action.position);
                        self.update_sequence(player_action.sequence.0);
                        return;
                    }
                    if let Some(server_arc) = world.server.upgrade() {
                        let mut abort_event = crate::plugin::api::events::block::block_damage_abort::BlockDamageAbortEvent::new(
                            player.clone(),
                            player_action.position,
                            world.clone(),
                            player.inventory().held_item(),
                        );
                        server_arc
                            .plugin_manager
                            .fire_blocking(&server_arc, &mut abort_event);
                    }

                    player.stop_mining();
                    self.update_sequence(player_action.sequence.0);
                }
                Status::FinishedDigging => {
                    let location = player_action.position;
                    if !player.can_interact_with_block_at(&location, 1.0) {
                        warn!(
                            "Player {0} tried to interact with block out of reach at {1}",
                            player.gameprofile.name, player_action.position
                        );
                        self.update_sequence(player_action.sequence.0);
                        return;
                    }

                    // A STOP for another position must not destroy the active target. Vanilla
                    // keeps that state so a later STOP for the original block can finish it.
                    let active_pos = *player
                        .mining_pos
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let active = player.mining.load(Ordering::Relaxed);
                    let delayed_pos = *player
                        .delayed_destroy_pos
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let delayed = player.delayed_destroy.load(Ordering::Relaxed);
                    let matches_destroy =
                        (active && active_pos == location) || (delayed && delayed_pos == location);
                    let entity = &player.get_entity();
                    let world = entity.world.load_full();
                    if !matches_destroy {
                        self.sync_block_state_to_client(&world, location);
                        self.update_sequence(player_action.sequence.0);
                        return;
                    }
                    if delayed {
                        self.update_sequence(player_action.sequence.0);
                        return;
                    }
                    if location.0.y > world.get_top_y() || !player.may_build() {
                        player.stop_mining();
                        self.sync_block_state_to_client(&world, location);
                        self.update_sequence(player_action.sequence.0);
                        return;
                    }

                    let (block, state) = world.get_block_and_state(&location);
                    if state.is_air() {
                        player.stop_mining();
                        self.sync_block_state_to_client(&world, location);
                        self.update_sequence(player_action.sequence.0);
                        return;
                    }
                    let elapsed = player
                        .tick_counter
                        .load(Ordering::Relaxed)
                        .checked_sub(player.start_mining_time.load(Ordering::Relaxed))
                        .unwrap_or(-1);
                    let speed = block::calc_block_breaking(player, state, block);
                    if !speed.is_finite() || speed < 0.0 || elapsed < 0 {
                        player.stop_mining();
                        self.sync_block_state_to_client(&world, location);
                        self.update_sequence(player_action.sequence.0);
                        return;
                    }
                    if !can_finish_digging(true, active_pos, location, elapsed, speed) {
                        player.begin_delayed_destroy(
                            location,
                            player.start_mining_time.load(Ordering::Relaxed),
                        );
                        self.update_sequence(player_action.sequence.0);
                        return;
                    }

                    player.stop_mining();
                    let block_drop = player.gamemode.load() != GameMode::Creative
                        && player.can_harvest(state, block);

                    let new_state = world.break_block(
                        &location,
                        Some(player),
                        if block_drop {
                            BlockFlags::NOTIFY_ALL
                        } else {
                            BlockFlags::SKIP_DROPS | BlockFlags::NOTIFY_ALL
                        },
                    );
                    if new_state.is_some() {
                        server
                            .block_registry
                            .broken(&world, block, player, &location, server, state);

                        player.apply_tool_damage_for_block_break(state);
                        if block_drop {
                            player.add_exhaustion(MINE_BLOCK_EXHAUSTION);
                        }
                        let item_id = player.inventory().held_item().item.id;
                        player.increment_stat(StatisticCategory::Used, item_id as i32, 1);
                        player.increment_stat(
                            StatisticCategory::Mined,
                            block.id.as_u16() as i32,
                            1,
                        );
                    }

                    self.sync_block_state_to_client(&world, location);

                    self.update_sequence(player_action.sequence.0);
                }
                Status::DropItem => {
                    player.drop_held_item(false);
                }
                Status::DropItemStack => {
                    player.drop_held_item(true);
                }
                Status::ReleaseItemInUse => {
                    let item_in_use = player
                        .living_entity
                        .item_in_use
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    if let Some(stack) = item_in_use {
                        server.item_registry.on_stopped_using(&stack, player);
                    }

                    player.living_entity.clear_active_hand();
                }
                Status::SwapItem => {
                    player.swap_item();
                }
                Status::SpearJab => {
                    if player.gamemode.load() == GameMode::Spectator {
                        return;
                    }

                    let stack = player.inventory().held_item();
                    server.item_registry.on_spear_jab(&stack, player);
                }
            },
            Err(_) => self.try_kick(&TextComponent::text("Invalid status")),
        }
    }

    pub fn update_sequence(&self, sequence: i32) {
        if sequence < 0 {
            error!("Expected packet sequence >= 0");
        }
        self.packet_sequence.store(
            self.packet_sequence.load(Ordering::Relaxed).max(sequence),
            Ordering::Relaxed,
        );
    }

    fn sync_block_state_to_client(&self, world: &World, position: BlockPos) {
        let synced_state_id = world.get_block_state_id(&position);
        self.try_send_packet(&CBlockUpdate::new(
            position,
            VarInt(i32::from(synced_state_id.as_u16())),
        ));
    }
}

/// Vanilla 26.2 accepts STOP once the server-side progress reaches 0.7; it does
/// not accept a STOP without the matching START state.
fn can_finish_digging(
    mining: bool,
    mining_pos: BlockPos,
    requested_pos: BlockPos,
    elapsed: i32,
    speed: f32,
) -> bool {
    mining
        && mining_pos == requested_pos
        && elapsed >= 0
        && speed.is_finite()
        && speed * elapsed.saturating_add(1) as f32 >= 0.7
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use pumpkin_config::{AdvancedConfiguration, BasicConfiguration, TelemetryConfig};
    use pumpkin_data::item_stack::ItemStack;
    use pumpkin_protocol::codec::item_stack_seralizer::ItemStackSerializer;
    use pumpkin_protocol::java::server::play::{SPlayerAction, SSetCreativeSlot};
    use pumpkin_util::math::{position::BlockPos, vector3::Vector3};
    use pumpkin_world::world::BlockFlags;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::net::{TcpListener, TcpStream};
    use uuid::Uuid;

    fn test_vanilla_data() -> crate::data::VanillaData {
        crate::data::VanillaData {
            banned_ip_list: std::sync::RwLock::new(Default::default()),
            banned_player_list: std::sync::RwLock::new(Default::default()),
            operator_config: std::sync::RwLock::new(Default::default()),
            user_cache: std::sync::RwLock::new(Default::default()),
            whitelist_config: std::sync::RwLock::new(Default::default()),
        }
    }

    async fn runtime_java_client(
        profile: &crate::net::GameProfile,
    ) -> Arc<crate::net::ClientPlatform> {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("player-action fixture listener");
        let address = listener
            .local_addr()
            .expect("player-action fixture address");
        let connector = tokio::spawn(TcpStream::connect(address));
        let (server_stream, peer_address) = listener
            .accept()
            .await
            .expect("player-action fixture accept");
        let _peer = connector
            .await
            .expect("player-action connector task")
            .expect("player-action fixture connect");
        let pending = crate::net::java::pending::PendingConnection::new(
            server_stream,
            peer_address,
            1,
            crate::net::PacketRateLimiter::new(false, 0.0, 0.0),
        );
        Arc::new(crate::net::ClientPlatform::Java(
            crate::net::java::JavaClient::from_pending(
                pending,
                profile.clone(),
                crate::net::PlayerConfig::default(),
            ),
        ))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn player_action_and_creative_slot_use_real_client_authority_and_stop_cutoff() {
        let temp_world = TempDir::new().expect("temporary runtime world");
        let mut basic = BasicConfiguration::default();
        basic.default_level_name = temp_world.path().to_string_lossy().into_owned();
        basic.allow_nether = false;
        basic.allow_end = false;
        basic.allow_chat_reports = false;
        basic.spawn_protection = 0;
        basic.use_favicon = false;

        let mut advanced = AdvancedConfiguration::default();
        advanced.logging.enabled = false;
        advanced.plugins.enabled = false;
        advanced.commands.use_console = false;
        advanced.commands.use_tty = false;
        advanced.networking.java.enabled = false;
        advanced.networking.bedrock.enabled = false;
        advanced.networking.query.enabled = false;
        advanced.networking.lan_broadcast.enabled = false;
        let server = crate::server::Server::new(
            basic,
            advanced,
            TelemetryConfig {
                enabled: false,
                ..TelemetryConfig::default()
            },
            test_vanilla_data(),
        )
        .await;
        let profile = crate::net::GameProfile {
            id: Uuid::from_u128(0x2620_0004),
            name: "player_action_fixture".to_owned(),
            properties: ArcSwap::from_pointee(Vec::new()),
            profile_actions: None,
        };
        let (player, world) = server
            .add_player(
                runtime_java_client(&profile).await,
                profile,
                Some(crate::net::PlayerConfig::default()),
            )
            .expect("fixture player published");
        player.client_loaded.store(true, Ordering::Relaxed);

        let position = BlockPos(Vector3::new(0, 100, 0));
        world
            .level
            .get_or_fetch_chunk(position.chunk_position(), |_| ())
            .await;
        world.set_block_state(
            &position,
            pumpkin_data::Block::DIAMOND_ORE.default_state.id,
            BlockFlags::FORCE_STATE,
        );
        let java = player.client.java().expect("Java client fixture");
        let packet_at = |status, position| SPlayerAction {
            status: pumpkin_protocol::codec::var_int::VarInt(status),
            position,
            face: 0,
            sequence: pumpkin_protocol::codec::var_int::VarInt(1),
        };
        let packet = |status| packet_at(status, position);

        // STOP without a server-side START must not mutate the real world.
        java.handle_player_action(&player, &packet(2), &server);
        assert_eq!(
            world.get_block_state(&position),
            pumpkin_data::Block::DIAMOND_ORE.default_state
        );

        // A non-building player is rejected before START creates mining state.
        player
            .abilities
            .lock()
            .expect("abilities lock")
            .allow_modify_world = false;
        java.handle_player_action(&player, &packet(0), &server);
        assert!(!player.mining.load(Ordering::Relaxed));
        assert_eq!(
            world.get_block_state(&position),
            pumpkin_data::Block::DIAMOND_ORE.default_state
        );

        player
            .abilities
            .lock()
            .expect("abilities lock")
            .allow_modify_world = true;

        // A mismatched STOP must preserve the active target for a later matching STOP.
        let other_position = BlockPos(Vector3::new(0, 100, 1));
        player.mining.store(true, Ordering::Relaxed);
        *player.mining_pos.lock().expect("mining position lock") = position;
        player.start_mining_time.store(0, Ordering::Relaxed);
        player.tick_counter.store(0, Ordering::Relaxed);
        java.handle_player_action(&player, &packet_at(2, other_position), &server);
        assert!(player.mining.load(Ordering::Relaxed));

        // A matching STOP below 0.7 enters vanilla delayed destroy instead of
        // clearing the state and losing the eventual break.
        java.handle_player_action(&player, &packet(2), &server);
        assert!(!player.mining.load(Ordering::Relaxed));
        assert!(player.delayed_destroy.load(Ordering::Relaxed));
        assert_eq!(
            world.get_block_state(&position),
            pumpkin_data::Block::DIAMOND_ORE.default_state
        );

        // The shared Player tick owns delayed completion; no further action packet is needed.
        // The exact threshold is derived by the runtime mining-speed calculation.
        for _ in 0..5_000 {
            if !player.delayed_destroy.load(Ordering::Relaxed) {
                break;
            }
            player.tick(&server);
        }
        assert!(!player.delayed_destroy.load(Ordering::Relaxed));
        assert_eq!(
            world.get_block_state(&position),
            pumpkin_data::Block::AIR.default_state
        );
        assert_eq!(
            player.get_stat(
                pumpkin_data::statistic::StatisticCategory::Mined,
                pumpkin_data::Block::DIAMOND_ORE.id.as_u16() as i32,
            ),
            1
        );
        assert_eq!(
            player.get_stat(
                pumpkin_data::statistic::StatisticCategory::Mined,
                pumpkin_data::Block::DIAMOND_ORE.default_state.id.as_u16() as i32,
            ),
            0
        );

        let empty_slot_packet =
            |slot| SSetCreativeSlot::new(slot, ItemStackSerializer::from(ItemStack::EMPTY.clone()));
        let stone = || ItemStack::new(1, &pumpkin_data::item::Item::STONE);

        for gamemode in [
            pumpkin_util::GameMode::Survival,
            pumpkin_util::GameMode::Adventure,
            pumpkin_util::GameMode::Spectator,
        ] {
            player.gamemode.store(gamemode);
            player.inventory.set_held_item(stone());
            assert!(
                java.handle_set_creative_slot(&player, empty_slot_packet(36))
                    .is_ok(),
                "{gamemode:?} creative-slot packet must be a no-op"
            );
            assert_eq!(
                player.inventory.held_item().item.id,
                pumpkin_data::item::Item::STONE.id
            );
        }

        player.gamemode.store(pumpkin_util::GameMode::Creative);
        player.inventory.set_held_item(stone());
        assert!(
            java.handle_set_creative_slot(&player, empty_slot_packet(36))
                .is_ok()
        );
        assert!(player.inventory.held_item().is_empty());

        for slot in [0, 46] {
            player.inventory.set_held_item(stone());
            assert!(
                java.handle_set_creative_slot(&player, empty_slot_packet(slot))
                    .is_ok(),
                "creative boundary slot {slot} must not mutate inventory"
            );
            assert_eq!(
                player.inventory.held_item().item.id,
                pumpkin_data::item::Item::STONE.id
            );
        }

        let composter_position = BlockPos(Vector3::new(1, 100, 0));
        world
            .level
            .get_or_fetch_chunk(composter_position.chunk_position(), |_| ())
            .await;
        world.set_block_state(
            &composter_position,
            pumpkin_data::Block::COMPOSTER.default_state.id,
            BlockFlags::FORCE_STATE,
        );
        player.gamemode.store(pumpkin_util::GameMode::Survival);
        player
            .inventory
            .set_held_item(ItemStack::new(1, &pumpkin_data::item::Item::WHEAT_SEEDS));
        let mut held_item = player.inventory.held_item();
        let cursor_pos = Vector3::new(0.5, 0.5, 0.5);
        let hit = crate::block::BlockHitResult {
            face: &pumpkin_data::BlockDirection::Up,
            cursor_pos: &cursor_pos,
        };
        let use_result = server.block_registry.use_with_item(
            &pumpkin_data::Block::COMPOSTER,
            &player,
            &composter_position,
            &hit,
            &mut held_item,
            &pumpkin_data::data_component_impl::EquipmentSlot::MAIN_HAND,
            &server,
            &world,
        );
        assert!(use_result.consumes_action());
        assert_eq!(held_item.item_count, 0);
        crate::net::bedrock::play::inventory_action::commit_held_item(&player, held_item);
        assert!(player.inventory.held_item().is_empty());
        assert_eq!(
            pumpkin_data::block_properties::ComposterLikeProperties::from_state_id(
                world.get_block_state_id(&composter_position),
            )
            .level,
            1
        );

        player.inventory.set_held_item(ItemStack::EMPTY.clone());
        player.gamemode.store(pumpkin_util::GameMode::Survival);
        world
            .remove_player(&player, crate::world::PlayerRemovalReason::Disconnect)
            .await;
        server.remove_player(&player);
    }
}
