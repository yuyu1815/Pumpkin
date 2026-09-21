#[allow(clippy::wildcard_imports)]
use super::*;

impl JavaClient {
    pub fn handle_move_vehicle(&self, player: &Arc<Player>, packet: &SMoveVehicle) {
        // Official 26.2 rejects NaN coordinates and non-finite rotations before
        // any authority or transform side effect. Infinity coordinates remain
        // legal here and are clamped below, matching containsInvalidValues.
        if packet.x.is_nan()
            || packet.y.is_nan()
            || packet.z.is_nan()
            || !packet.yaw.is_finite()
            || !packet.pitch.is_finite()
        {
            self.try_kick(&TextComponent::translate_cross(
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_VEHICLE_MOVEMENT,
                translation::java::MULTIPLAYER_DISCONNECT_INVALID_VEHICLE_MOVEMENT,
                [],
            ));
            return;
        }
        if !player.has_client_loaded()
            || player
                .awaiting_teleport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
        {
            return;
        }

        let entity = player.get_entity();
        let Some(mut vehicle) = entity.get_vehicle() else {
            return;
        };
        loop {
            let Some(parent) = vehicle.get_entity().get_vehicle() else {
                break;
            };
            vehicle = parent;
        }
        let controlling_passenger = vehicle
            .get_entity()
            .passengers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .first()
            .map(|passenger| passenger.get_entity().entity_id);
        if controlling_passenger != Some(entity.entity_id) {
            return;
        }

        // A movement packet was received this tick — tracked for SClientTickEnd zeroing.
        self.received_movement_this_tick
            .store(true, Ordering::Relaxed);
        let last_pos = entity.pos.load();
        let pos = Vector3::new(
            Self::clamp_horizontal(packet.x),
            Self::clamp_vertical(packet.y),
            Self::clamp_horizontal(packet.z),
        );
        let vehicle_entity = vehicle.get_entity();
        vehicle_entity.set_pos(pos);
        vehicle_entity.set_rotation(wrap_degrees(packet.yaw), wrap_degrees(packet.pitch));
        vehicle_entity
            .on_ground
            .store(packet.on_ground, Ordering::Relaxed);
        entity.set_pos(pos);
        let distance = last_pos.squared_distance_to_vec(&pos).sqrt();
        let cm = (distance * 100.0).round() as i32;
        if cm > 0 {
            let stat = player.get_movement_statistic();
            player.increment_stat(
                pumpkin_data::statistic::StatisticCategory::Custom,
                stat as i32,
                cm,
            );
        }
        chunker::update_position(player);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use pumpkin_config::{AdvancedConfiguration, BasicConfiguration, TelemetryConfig};
    use pumpkin_data::entity::EntityType;
    use pumpkin_protocol::java::server::play::SMoveVehicle;
    use pumpkin_util::math::vector3::Vector3;
    use std::net::SocketAddr;
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
            .expect("vehicle fixture listener");
        let address = listener
            .local_addr()
            .expect("vehicle fixture listener address");
        let connector = tokio::spawn(TcpStream::connect(address));
        let (server_stream, peer_address) =
            listener.accept().await.expect("vehicle fixture accept");
        connector
            .await
            .expect("vehicle fixture connector task")
            .expect("vehicle fixture connect");
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
    async fn vehicle_movement_rejects_or_clamps_invalid_coordinates() {
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
            id: Uuid::from_u128(0x2620_0005),
            name: "move_vehicle_fixture".to_owned(),
            properties: ArcSwap::from_pointee(Vec::new()),
            profile_actions: None,
        };
        let (player, world) = server
            .add_player(
                runtime_java_client(&profile).await,
                profile,
                Some(crate::net::PlayerConfig::default()),
            )
            .expect("vehicle fixture player published");
        player.client_loaded.store(true, Ordering::Relaxed);

        let initial = Vector3::new(0.0, 100.0, 0.0);
        let vehicle: Arc<dyn crate::entity::EntityBase> =
            Arc::new(crate::entity::vehicle::boat::BoatEntity::new(
                crate::entity::Entity::new(world.clone(), initial, &EntityType::OAK_BOAT),
            ));
        let decoy: Arc<dyn crate::entity::EntityBase> =
            Arc::new(crate::entity::vehicle::boat::BoatEntity::new(
                crate::entity::Entity::new(world.clone(), initial, &EntityType::OAK_BOAT),
            ));
        vehicle.get_entity().add_passenger(vehicle.clone(), decoy);
        vehicle
            .get_entity()
            .add_passenger(vehicle.clone(), player.clone());
        let java = player.client.java().expect("Java client fixture");
        let packet = SMoveVehicle {
            x: f64::NAN,
            y: initial.y,
            z: initial.z,
            yaw: 45.0,
            pitch: 10.0,
            on_ground: false,
        };

        java.handle_move_vehicle(&player, &packet);

        // Official containsInvalidValues rejects NaN before any vehicle/player
        // transform or rotation side effect. This is intentionally red before
        // the production guard is applied.
        assert_eq!(player.get_entity().pos.load(), initial);
        assert_eq!(vehicle.get_entity().pos.load(), initial);
        assert_eq!(vehicle.get_entity().yaw.load(), 0.0);
        assert_eq!(vehicle.get_entity().pitch.load(), 0.0);

        let finite_packet = SMoveVehicle {
            x: 10.0,
            y: initial.y,
            z: 10.0,
            yaw: 45.0,
            pitch: 10.0,
            on_ground: false,
        };
        java.handle_move_vehicle(&player, &finite_packet);
        assert_eq!(vehicle.get_entity().pos.load(), initial);
        assert_eq!(player.get_entity().pos.load(), initial);
    }
}
