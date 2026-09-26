use super::Server;
use crate::net::java::{JavaClient, pending::PendingConnection};
use crate::net::{ClientPlatform, GameProfile, PacketRateLimiter, PlayerConfig};
use arc_swap::ArcSwap;
use pumpkin_config::{AdvancedConfiguration, BasicConfiguration, TelemetryConfig};
use pumpkin_data::dimension::Dimension;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
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

async fn runtime_java_client(profile: &GameProfile) -> Arc<ClientPlatform> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("fixture listener");
    let address = listener.local_addr().expect("fixture listener address");
    let connector = tokio::spawn(TcpStream::connect(address));
    let (stream, peer_address) = listener.accept().await.expect("fixture accept");
    let _peer = connector
        .await
        .expect("connector task")
        .expect("fixture connect");
    let pending = PendingConnection::new(
        stream,
        peer_address,
        1,
        PacketRateLimiter::new(false, 0.0, 0.0),
    );
    Arc::new(ClientPlatform::Java(JavaClient::from_pending(
        pending,
        profile.clone(),
        PlayerConfig::default(),
    )))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_signal_follows_disconnect_not_dimension_transfer() {
    let temp_world = TempDir::new().expect("temporary world");
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
    advanced.networking.java.enabled = false;
    advanced.networking.bedrock.enabled = false;
    advanced.networking.query.enabled = false;
    advanced.networking.lan_broadcast.enabled = false;
    advanced.networking.rcon.enabled = false;
    let server = Server::new(
        basic,
        advanced,
        TelemetryConfig {
            enabled: false,
            ..TelemetryConfig::default()
        },
        test_vanilla_data(),
    )
    .await;

    let player_id = Uuid::new_v4();
    let profile = GameProfile {
        id: player_id,
        name: "chat_lifecycle".to_string(),
        properties: ArcSwap::from_pointee(Vec::new()),
        profile_actions: None,
    };
    let (player, overworld) = server
        .add_player(runtime_java_client(&profile).await, profile, None)
        .expect("player published");
    let admission = player.admission.clone();
    let nether = server.get_world_from_dimension(&Dimension::THE_NETHER);
    {
        let transfer_signal = admission.removed.notified();
        tokio::pin!(transfer_signal);
        transfer_signal.as_mut().enable();
        overworld
            .remove_player(
                &player,
                crate::world::PlayerRemovalReason::DimensionTransfer,
            )
            .await
            .expect("player detached for transfer");
        player.change_world_chunks(&overworld.level, &nether);
        player.living_entity.entity.set_world(nether.clone());
        nether.publish_player_membership(&player);

        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut transfer_signal)
                .await
                .is_err(),
            "dimension transfer must not signal duplicate-login admission"
        );
    }
    assert!(
        admission
            .players()
            .iter()
            .any(|candidate| Arc::ptr_eq(candidate, &player))
    );

    let disconnect_signal = admission.removed.notified();
    tokio::pin!(disconnect_signal);
    disconnect_signal.as_mut().enable();
    nether
        .remove_player(&player, crate::world::PlayerRemovalReason::Disconnect)
        .await;
    tokio::time::timeout(Duration::from_secs(1), &mut disconnect_signal)
        .await
        .expect("true disconnect signals duplicate-login admission");
    assert!(admission.players().is_empty());
    server.remove_player(&player);
}
