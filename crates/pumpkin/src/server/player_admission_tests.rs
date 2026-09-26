use std::{net::SocketAddr, sync::Arc, time::Duration};

use arc_swap::ArcSwap;
use pumpkin_config::{AdvancedConfiguration, BasicConfiguration, TelemetryConfig};
use pumpkin_data::packet::CURRENT_MC_VERSION;
use pumpkin_protocol::ConnectionState;
use pumpkin_util::text::TextComponent;
use tempfile::TempDir;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Barrier, oneshot},
    task::JoinHandle,
    time::timeout,
};
use uuid::Uuid;

use crate::{
    entity::player::Player,
    net::{ClientPlatform, GameProfile, PacketRateLimiter, PlayerConfig},
    net::java::{JavaClient, pending::PendingConnection},
    server::Server,
    world::PlayerRemovalReason,
};

fn test_vanilla_data() -> crate::data::VanillaData {
    crate::data::VanillaData {
        banned_ip_list: std::sync::RwLock::new(Default::default()),
        banned_player_list: std::sync::RwLock::new(Default::default()),
        operator_config: std::sync::RwLock::new(Default::default()),
        user_cache: std::sync::RwLock::new(Default::default()),
        whitelist_config: std::sync::RwLock::new(Default::default()),
    }
}

async fn test_server(temp_world: &TempDir) -> Arc<Server> {
    test_server_with_limits(temp_world, true, 1, false, 0).await
}

async fn test_server_with_limits(
    temp_world: &TempDir,
    java_enabled: bool,
    java_max_players: u32,
    bedrock_enabled: bool,
    bedrock_max_players: u32,
) -> Arc<Server> {
    let mut basic = BasicConfiguration::default();
    basic.default_level_name = temp_world.path().to_string_lossy().into_owned();
    basic.allow_nether = true;
    basic.allow_end = false;
    basic.allow_chat_reports = false;
    basic.use_favicon = false;

    let mut advanced = AdvancedConfiguration::default();
    advanced.logging.enabled = false;
    advanced.plugins.enabled = false;
    advanced.commands.use_console = false;
    advanced.commands.use_tty = false;
    advanced.networking.java.enabled = java_enabled;
    advanced.networking.java.max_players = java_max_players;
    advanced.networking.bedrock.enabled = bedrock_enabled;
    advanced.networking.bedrock.max_players = bedrock_max_players;
    advanced.networking.query.enabled = false;
    advanced.networking.lan_broadcast.enabled = false;
    advanced.networking.rcon.enabled = false;

    Server::new(
        basic,
        advanced,
        TelemetryConfig {
            enabled: false,
            ..TelemetryConfig::default()
        },
        test_vanilla_data(),
    )
    .await
}

fn profile(id: Uuid) -> GameProfile {
    GameProfile {
        id,
        name: "admission_test".to_string(),
        properties: ArcSwap::from_pointee(Vec::new()),
        profile_actions: None,
    }
}

async fn java_client(
    profile: &GameProfile,
    id: u64,
) -> (Arc<ClientPlatform>, TcpStream) {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("fixture listener");
    let address = listener.local_addr().expect("fixture listener address");
    let connector = tokio::spawn(TcpStream::connect(address));
    let (server_stream, peer_address) = listener.accept().await.expect("fixture accept");
    let peer = connector
        .await
        .expect("fixture connector task")
        .expect("fixture connect");
    let pending = PendingConnection::new(
        server_stream,
        peer_address,
        id,
        PacketRateLimiter::new(false, 0.0, 0.0),
    );
    let mut java = JavaClient::from_pending(
        pending,
        profile.clone(),
        PlayerConfig::default(),
    );
    java.version.store(CURRENT_MC_VERSION);
    java.connection_state.store(ConnectionState::Play);
    java.start_outgoing_packet_task();
    (Arc::new(ClientPlatform::Java(java)), peer)
}

fn disconnect_cleanup(
    server: Arc<Server>,
    player: Arc<Player>,
    delay: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        player.client.await_close_interrupt().await;
        tokio::time::sleep(delay).await;
        let world = player.world();
        world
            .remove_player(&player, PlayerRemovalReason::Disconnect)
            .await;
        server.remove_player(&player);
        if let ClientPlatform::Java(client) = player.client.as_ref() {
            client.await_tasks().await;
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_admission_waits_for_delayed_disconnect_removal() {
    let temp_world = TempDir::new().expect("temporary runtime world");
    let server = test_server(&temp_world).await;
    let id = Uuid::from_u128(0x21_0001);
    let profile = profile(id);
    let (old_client, _old_peer) = java_client(&profile, 1).await;
    let (old, old_world) = server
        .add_player(old_client.clone(), profile.clone(), Some(PlayerConfig::default()))
        .expect("old player published");
    let old_cleanup = disconnect_cleanup(
        server.clone(),
        old.clone(),
        Duration::from_millis(100),
    );

    let (new_client, _new_peer) = java_client(&profile, 2).await;
    let server_for_admission = server.clone();
    let mut admission = tokio::spawn(async move {
        server_for_admission
            .admit_player(new_client, profile, Some(PlayerConfig::default()))
            .await
    });

    assert!(timeout(Duration::from_millis(30), &mut admission).await.is_err());
    assert!(old_world
        .players
        .load()
        .iter()
        .any(|player| Arc::ptr_eq(player, &old)));

    let (replacement, _) = timeout(Duration::from_secs(3), admission)
        .await
        .expect("admission timed out")
        .expect("admission task failed")
        .expect("replacement was not admitted");
    old_cleanup.await.expect("old cleanup task failed");
    assert!(!old_world
        .players
        .load()
        .iter()
        .any(|player| Arc::ptr_eq(player, &old)));
    assert!(Arc::ptr_eq(
        &server
            .player_admission(id)
            .players()
            .into_iter()
            .next()
            .expect("replacement remains admitted"),
        &replacement
    ));

    let replacement_cleanup = disconnect_cleanup(
        server.clone(),
        replacement.clone(),
        Duration::ZERO,
    );
    replacement.client.try_kick(
        crate::net::DisconnectReason::Kicked,
        &TextComponent::text("test cleanup"),
    );
    replacement_cleanup
        .await
        .expect("replacement cleanup task failed");
}

async fn admit_concurrently(
    server: Arc<Server>,
    profile: GameProfile,
    client_id: u64,
    barrier: Arc<Barrier>,
) -> (Option<Arc<Player>>, JoinHandle<()>, TcpStream) {
    let (client, peer) = java_client(&profile, client_id).await;
    let cleanup_client = client.clone();
    let cleanup_server = server.clone();
    let (player_tx, player_rx) = oneshot::channel::<Option<Arc<Player>>>();
    let cleanup = tokio::spawn(async move {
        if let Ok(Some(player)) = player_rx.await {
            cleanup_client.await_close_interrupt().await;
            let world = player.world();
            world
                .remove_player(&player, PlayerRemovalReason::Disconnect)
                .await;
            cleanup_server.remove_player(&player);
            if let ClientPlatform::Java(client) = cleanup_client.as_ref() {
                client.await_tasks().await;
            }
        }
    });

    barrier.wait().await;
    let admitted = server
        .admit_player(client.clone(), profile, Some(PlayerConfig::default()))
        .await;
    if let Some((player, _)) = &admitted {
        assert!(player_tx.send(Some(player.clone())).is_ok());
    } else {
        let _ = player_tx.send(None);
    }
    (admitted.map(|(player, _)| player), cleanup, peer)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_different_uuids_with_same_name_only_admit_one() {
    let temp_world = TempDir::new().expect("temporary runtime world");
    let server = test_server(&temp_world).await;
    let mut first_profile = profile(Uuid::from_u128(0x21_0003));
    first_profile.name = "SameName".to_string();
    let mut second_profile = profile(Uuid::from_u128(0x21_0004));
    second_profile.name = "samename".to_string();
    let barrier = Arc::new(Barrier::new(2));

    let first = tokio::spawn(admit_concurrently(
        server.clone(),
        first_profile,
        12,
        barrier.clone(),
    ));
    let second = tokio::spawn(admit_concurrently(
        server.clone(),
        second_profile,
        13,
        barrier,
    ));
    let (first, second) = tokio::join!(first, second);
    let (first, first_cleanup, _first_peer) = first.expect("first admission task failed");
    let (second, second_cleanup, _second_peer) = second.expect("second admission task failed");

    assert_ne!(first.is_some(), second.is_some(), "exactly one name may be admitted");
    let winner = first.or(second).expect("one admission should succeed");
    winner.client.try_kick(
        crate::net::DisconnectReason::Kicked,
        &TextComponent::text("test cleanup"),
    );
    timeout(Duration::from_secs(3), async {
        first_cleanup.await.expect("first cleanup task failed");
        second_cleanup.await.expect("second cleanup task failed");
    })
    .await
    .expect("disconnect cleanup timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_bypass_admits_at_capacity_but_normal_player_is_rejected() {
    let temp_world = TempDir::new().expect("temporary runtime world");
    let server = test_server(&temp_world).await;
    let mut first_profile = profile(Uuid::from_u128(0x21_0010));
    first_profile.name = "first".to_string();
    let (first_client, _first_peer) = java_client(&first_profile, 30).await;
    let (first, _world) = server
        .admit_player(
            first_client,
            first_profile,
            Some(PlayerConfig::default()),
        )
        .await
        .expect("first normal player admitted");

    let mut rejected_profile = profile(Uuid::from_u128(0x21_0011));
    rejected_profile.name = "second".to_string();
    let (rejected_client, _rejected_peer) = java_client(&rejected_profile, 31).await;
    assert!(server
        .admit_player(
            rejected_client.clone(),
            rejected_profile,
            Some(PlayerConfig::default()),
        )
        .await
        .is_none());
    assert!(rejected_client.closed());

    let mut operator_profile = profile(Uuid::from_u128(0x21_0012));
    operator_profile.name = "operator".to_string();
    server
        .data
        .operator_config
        .write()
        .unwrap()
        .ops
        .push(pumpkin_config::op::Op::new(
            operator_profile.id,
            operator_profile.name.clone(),
            pumpkin_util::permission::PermissionLvl::Four,
            true,
        ));
    let (operator_client, _operator_peer) = java_client(&operator_profile, 32).await;
    let (operator, _world) = server
        .admit_player(
            operator_client,
            operator_profile,
            Some(PlayerConfig::default()),
        )
        .await
        .expect("configured operator bypasses full capacity");
    assert_eq!(server.get_player_count(), 2);

    for player in [first, operator] {
        player.client.try_kick(
            crate::net::DisconnectReason::Kicked,
            &TextComponent::text("test cleanup"),
        );
        let world = player.world();
        world.remove_player(&player, PlayerRemovalReason::Disconnect).await;
        server.remove_player(&player);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_distinct_uuids_respect_max_players_capacity() {
    let temp_world = TempDir::new().expect("temporary runtime world");
    let server = test_server(&temp_world).await;
    let mut first_profile = profile(Uuid::from_u128(0x21_0020));
    first_profile.name = "first_capacity".to_string();
    let mut second_profile = profile(Uuid::from_u128(0x21_0021));
    second_profile.name = "second_capacity".to_string();
    let barrier = Arc::new(Barrier::new(2));

    let first = tokio::spawn(admit_concurrently(
        server.clone(),
        first_profile,
        40,
        barrier.clone(),
    ));
    let second = tokio::spawn(admit_concurrently(
        server.clone(),
        second_profile,
        41,
        barrier,
    ));
    let (first, second) = tokio::join!(first, second);
    let (first, first_cleanup, _first_peer) = first.expect("first admission task failed");
    let (second, second_cleanup, _second_peer) = second.expect("second admission task failed");

    assert_ne!(first.is_some(), second.is_some(), "capacity one admits exactly one distinct UUID");
    assert_eq!(server.get_player_count(), 1);
    if let Some(player) = first.or(second) {
        player.client.try_kick(
            crate::net::DisconnectReason::Kicked,
            &TextComponent::text("test cleanup"),
        );
    }
    timeout(Duration::from_secs(3), async {
        first_cleanup.await.expect("first cleanup task failed");
        second_cleanup.await.expect("second cleanup task failed");
    })
    .await
    .expect("disconnect cleanup timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_protocol_limits_use_strictest_enabled_server_wide_cap() {
    for (java_enabled, java_max, bedrock_enabled, bedrock_max, reject_second) in [
        (true, 1, true, 10, true),
        (true, 10, true, 1, true),
        (false, 1, true, 10, false),
        (true, 0, true, 0, false),
    ] {
        let temp_world = TempDir::new().expect("temporary runtime world");
        let server = test_server_with_limits(
            &temp_world,
            java_enabled,
            java_max,
            bedrock_enabled,
            bedrock_max,
        )
        .await;
        let mut first_profile = profile(Uuid::new_v4());
        first_profile.name = "mixed_first".to_string();
        let (first_client, _first_peer) = java_client(&first_profile, 50).await;
        let (first, _) = server
            .admit_player(first_client, first_profile, Some(PlayerConfig::default()))
            .await
            .expect("first player admitted");

        let mut second_profile = profile(Uuid::new_v4());
        second_profile.name = "mixed_second".to_string();
        let (second_client, _second_peer) = java_client(&second_profile, 51).await;
        let second = server
            .admit_player(second_client, second_profile, Some(PlayerConfig::default()))
            .await;
        assert_eq!(
            second.is_none(),
            reject_second,
            "limits Java={java_max} (enabled={java_enabled}), Bedrock={bedrock_max} (enabled={bedrock_enabled})"
        );
        assert_eq!(server.get_player_count(), if reject_second { 1 } else { 2 });

        let mut players = vec![first];
        if let Some((player, _)) = second {
            players.push(player);
        }
        for player in players {
            player.client.try_kick(
                crate::net::DisconnectReason::Kicked,
                &TextComponent::text("test cleanup"),
            );
            player
                .world()
                .remove_player(&player, PlayerRemovalReason::Disconnect)
                .await;
            server.remove_player(&player);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_duplicate_admissions_publish_only_the_last_session() {
    let temp_world = TempDir::new().expect("temporary runtime world");
    let server = test_server(&temp_world).await;
    let profile = profile(Uuid::from_u128(0x21_0002));
    let barrier = Arc::new(Barrier::new(2));

    let first = tokio::spawn(admit_concurrently(
        server.clone(),
        profile.clone(),
        10,
        barrier.clone(),
    ));
    let second = tokio::spawn(admit_concurrently(
        server.clone(),
        profile.clone(),
        11,
        barrier,
    ));
    let (first, second) = tokio::join!(first, second);
    let (first, first_cleanup, _first_peer) = first.expect("first admission task failed");
    let (second, second_cleanup, _second_peer) = second.expect("second admission task failed");

    let active = server.player_admission(profile.id).players();
    assert_eq!(active.len(), 1, "only one session may remain published");
    let winner = active.into_iter().next().expect("published session");
    assert!(first
        .iter()
        .chain(second.iter())
        .any(|candidate| Arc::ptr_eq(candidate, &winner)));

    winner.client.try_kick(
        crate::net::DisconnectReason::Kicked,
        &TextComponent::text("test cleanup"),
    );
    timeout(Duration::from_secs(3), async {
        first_cleanup.await.expect("first cleanup task failed");
        second_cleanup.await.expect("second cleanup task failed");
    })
    .await
    .expect("disconnect cleanup timed out");
}
