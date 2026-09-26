use std::{net::SocketAddr, num::NonZero, sync::Arc};

use bytes::Bytes;
use crossbeam::atomic::AtomicCell;
use pumpkin_config::networking::compression::CompressionInfo;
use pumpkin_data::packet::CURRENT_MC_VERSION;
use pumpkin_protocol::{
    ClientPacket, ConnectionState, PacketDecodeError, RawPacket, ServerPacket,
    java::{
        client::config::CConfigDisconnect,
        client::login::CLoginDisconnect,
        client::play::CPlayDisconnect,
        packet_decoder::TCPNetworkDecoder,
        packet_encoder::TCPNetworkEncoder,
        server::{
            config::{
                SAcceptCodeOfConduct, SAcknowledgeFinishConfig, SClientInformationConfig,
                SConfigCookieResponse, SConfigPong, SConfigResourcePack, SKnownPacks,
                SPluginMessage,
            },
            status::SStatusPingRequest,
        },
    },
    packet::MultiVersionJavaPacket,
    ser::ReadingError,
};
use pumpkin_util::{Hand, text::TextComponent, version::JavaMinecraftVersion};
use tokio::{
    io::{BufReader, BufWriter},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::{
    entity::player::ChatMode,
    net::{
        EncryptionError, GameProfile, PacketHandlerResult, PacketRateLimiter, PlayerConfig,
        can_not_join,
    },
    server::Server,
};

use super::{ConfigurationPhase, JavaClient, LoginProtocolPhase, require_empty_body};

const BRAND_CHANNEL_PREFIX: &str = "minecraft:brand";

/// How long a connection may stay silent before login finishes.
///
/// Once a player is in game, [`JavaClient::progress_player_packets`] keeps the
/// connection honest with keep-alives. Nothing plays that role beforehand, and
/// accepted sockets have no TCP keep-alive either, so a peer that stops talking
/// without closing would otherwise hold its descriptor for the lifetime of the
/// server. The timer covers silence rather than the whole handshake: it is reset
/// on every packet, so a slow but progressing login is never cut off.
const HANDSHAKE_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

enum DecodedStatusPacket {
    Request,
    Ping(SStatusPingRequest),
}

fn decode_status_packet(
    packet: &RawPacket,
    version: JavaMinecraftVersion,
) -> Result<DecodedStatusPacket, ReadingError> {
    let mut payload = &packet.payload[..];

    match packet.id {
        id if id == pumpkin_protocol::java::server::status::SStatusRequest::to_id(version) => {
            pumpkin_protocol::java::server::status::SStatusRequest::read(&mut payload, &version)?;
            require_empty_body(payload, "status request")?;
            Ok(DecodedStatusPacket::Request)
        }
        id if id == SStatusPingRequest::to_id(version) => {
            let ping = SStatusPingRequest::read(&mut payload, &version)?;
            require_empty_body(payload, "status ping")?;
            Ok(DecodedStatusPacket::Ping(ping))
        }
        _ => Err(ReadingError::Message(format!(
            "Failed to handle java client packet id {} in Status State",
            packet.id
        ))),
    }
}

pub struct PendingConnection {
    pub id: u64,
    pub address: SocketAddr,
    pub server_address: String,
    pub version: AtomicCell<JavaMinecraftVersion>,
    pub connection_state: AtomicCell<ConnectionState>,
    pub(crate) configuration_phase: AtomicCell<ConfigurationPhase>,
    pub(crate) login_protocol_phase: AtomicCell<LoginProtocolPhase>,
    pub(crate) known_packs_state: super::KnownPacksState,
    pub close_token: CancellationToken,
    pub network_writer: TCPNetworkEncoder<BufWriter<OwnedWriteHalf>>,
    pub network_reader: TCPNetworkDecoder<BufReader<OwnedReadHalf>>,
    pub gameprofile: Option<GameProfile>,
    pub(crate) online_profile_verified: bool,
    pub config: Option<PlayerConfig>,
    pub brand: Option<String>,
    pub packet_limiter: PacketRateLimiter,
    pub verify_token: Option<[u8; 4]>,
    pub vine_challenge: Option<[u8; 16]>,
    pub(crate) velocity_message_id: Option<i32>,
}

impl PendingConnection {
    pub(crate) fn remember_known_packs(&self, request: &[pumpkin_protocol::KnownPack<'_>]) {
        super::remember_known_packs(&self.known_packs_state, request);
    }

    pub(crate) fn known_packs_selection(
        &self,
        response: &[pumpkin_protocol::KnownPack<'_>],
    ) -> super::KnownPacksSelection {
        super::select_known_packs(&self.known_packs_state, response)
    }

    #[must_use]
    pub fn new(
        tcp_stream: TcpStream,
        address: SocketAddr,
        id: u64,
        packet_limiter: PacketRateLimiter,
    ) -> Self {
        let (read, write) = tcp_stream.into_split();
        Self {
            id,
            address,
            server_address: String::new(),
            version: AtomicCell::new(CURRENT_MC_VERSION),
            connection_state: AtomicCell::new(ConnectionState::HandShake),
            configuration_phase: AtomicCell::new(ConfigurationPhase::NotInConfiguration),
            login_protocol_phase: AtomicCell::new(LoginProtocolPhase::AwaitingLoginStart),
            known_packs_state: std::sync::Arc::new(std::sync::Mutex::new(None)),
            close_token: CancellationToken::new(),
            network_writer: TCPNetworkEncoder::new(BufWriter::new(write)),
            network_reader: TCPNetworkDecoder::new(BufReader::new(read)),
            gameprofile: None,
            online_profile_verified: false,
            config: None,
            brand: None,
            packet_limiter,
            verify_token: None,
            vine_challenge: None,
            velocity_message_id: None,
        }
    }

    pub fn close(&self) {
        self.close_token.cancel();
    }

    pub fn is_closed(&self) -> bool {
        self.close_token.is_cancelled()
    }

    pub async fn await_close_interrupt(&self) {
        self.close_token.cancelled().await;
    }

    pub fn set_encryption(&mut self, shared_secret: &[u8]) -> Result<(), EncryptionError> {
        let crypt_key: [u8; 16] = shared_secret
            .try_into()
            .map_err(|_| EncryptionError::SharedWrongLength)?;
        self.network_reader
            .set_encryption(&crypt_key)
            .map_err(|_| EncryptionError::AlreadyEncrypted)?;
        self.network_writer
            .set_encryption(&crypt_key)
            .map_err(|_| EncryptionError::AlreadyEncrypted)?;
        Ok(())
    }

    pub fn set_compression(&mut self, compression: &CompressionInfo) {
        if compression.level > 9 {
            error!("Invalid compression level! Clients will not be able to read this!");
        }

        self.network_reader
            .set_compression(compression.threshold as usize);

        self.network_writer
            .set_compression((compression.threshold as usize, compression.level));
    }

    pub async fn get_packet(&mut self) -> Option<RawPacket> {
        let close_token = self.close_token.clone();
        let packet_result = tokio::select! {
            () = close_token.cancelled() => {
                debug!("Canceling pending connection packet processing");
                return None;
            },
            () = tokio::time::sleep(HANDSHAKE_IDLE_TIMEOUT) => {
                debug!(
                    "Client {} sent nothing for {}s before finishing login, dropping it",
                    self.id,
                    HANDSHAKE_IDLE_TIMEOUT.as_secs()
                );
                return None;
            },
            res = self.network_reader.get_raw_packet() => res,
        };

        match packet_result {
            Ok(packet) => Some(packet),
            Err(err) => {
                if !matches!(err, PacketDecodeError::ConnectionClosed) {
                    debug!("Failed to decode packet from client {}: {}", self.id, err);
                    let text = format!("Error while reading incoming packet {err}");
                    self.kick(TextComponent::text(text)).await;
                }
                None
            }
        }
    }

    pub async fn send_packet_now<P: ClientPacket>(&mut self, packet: &P) {
        let mut packet_buf = Vec::new();
        if let Err(err) =
            JavaClient::write_packet_for_version(packet, self.version.load(), &mut packet_buf)
        {
            error!("Failed to write packet: {err:?}");
            return;
        }
        let payload = Bytes::from(packet_buf);
        if let Err(err) = self.network_writer.write_packet(payload).await {
            warn!("Failed to send packet to client {}: {}", self.id, err);
        }
        let _ = self.network_writer.flush().await;
    }

    pub async fn kick(&mut self, reason: TextComponent) {
        match self.connection_state.load() {
            ConnectionState::Login => {
                self.send_packet_now(&CLoginDisconnect::new(
                    serde_json::to_string(&reason.0).unwrap_or_else(|_| String::new()),
                ))
                .await;
            }
            ConnectionState::Config => {
                self.send_packet_now(&CConfigDisconnect::new(&reason))
                    .await;
            }
            ConnectionState::Play => {
                self.send_packet_now(&CPlayDisconnect::new(&reason)).await;
            }
            _ => {}
        }
        debug!("Closing connection for {}", self.id);
        self.close();
    }

    pub async fn handle_login_sequence(&mut self, server: &Arc<Server>) -> PacketHandlerResult {
        while let Some(packet) = self.get_packet().await {
            if !self.packet_limiter.check_packet() {
                warn!(
                    "Pending client {} exceeded packet rate limit (rate: {}/s)",
                    self.id,
                    self.packet_limiter.max_rate()
                );
                self.kick(TextComponent::text(
                    server
                        .advanced_config
                        .networking
                        .java
                        .packet_limiter
                        .kick_message
                        .clone(),
                ))
                .await;
                return PacketHandlerResult::Stop;
            }

            match self.handle_packet(server, &packet).await {
                Ok(result) => {
                    if let Some(result) = result {
                        return result;
                    }
                }
                Err(error) => {
                    let text = format!("Error while reading incoming packet {error}");
                    debug!(
                        "Failed to read incoming packet with id {}: {}",
                        packet.id, error
                    );
                    self.kick(TextComponent::text(text)).await;
                }
            }
        }
        PacketHandlerResult::Stop
    }

    pub async fn handle_packet(
        &mut self,
        server: &Arc<Server>,
        packet: &RawPacket,
    ) -> Result<Option<PacketHandlerResult>, ReadingError> {
        match self.connection_state.load() {
            ConnectionState::HandShake => self.handle_handshake_packet(server, packet).await,
            ConnectionState::Status => self.handle_status_packet(server, packet).await,
            ConnectionState::Login | ConnectionState::Transfer => {
                self.handle_login_packet(server, packet).await
            }
            ConnectionState::Config => self.handle_config_packet(server, packet).await,
            ConnectionState::Play => Ok(None),
        }
    }

    async fn handle_handshake_packet(
        &mut self,
        server: &Arc<Server>,
        packet: &RawPacket,
    ) -> Result<Option<PacketHandlerResult>, ReadingError> {
        debug!("Handling handshake group");
        let mut payload = &packet.payload[..];
        match packet.id {
            0 => {
                self.handle_handshake(
                    server,
                    pumpkin_protocol::java::server::handshake::SHandShake::read(
                        &mut payload,
                        &self.version.load(),
                    )?,
                )
                .await;
                Ok(None)
            }
            _ => Err(ReadingError::Message(format!(
                "Failed to handle packet id {} in Handshake State",
                packet.id
            ))),
        }
    }

    async fn handle_status_packet(
        &mut self,
        server: &Arc<Server>,
        packet: &RawPacket,
    ) -> Result<Option<PacketHandlerResult>, ReadingError> {
        debug!("Handling status group");
        let version = self.version.load();

        match decode_status_packet(packet, version)? {
            DecodedStatusPacket::Request => {
                self.handle_status_request(server).await;
            }
            DecodedStatusPacket::Ping(ping) => {
                self.handle_ping_request(ping).await;
            }
        }
        Ok(None)
    }

    async fn handle_login_packet(
        &mut self,
        server: &Arc<Server>,
        packet: &RawPacket,
    ) -> Result<Option<PacketHandlerResult>, ReadingError> {
        debug!("Handling login group");
        let mut payload = &packet.payload[..];
        let version = self.version.load();

        match packet.id {
            id if id == pumpkin_protocol::java::server::login::SLoginStart::to_id(version) => {
                Ok(self
                    .handle_login_start(
                        server,
                        pumpkin_protocol::java::server::login::SLoginStart::read(
                            &mut payload,
                            &version,
                        )?,
                    )
                    .await)
            }
            id if id
                == pumpkin_protocol::java::server::login::SEncryptionResponse::to_id(version) =>
            {
                Ok(self
                    .handle_encryption_response(
                        server,
                        pumpkin_protocol::java::server::login::SEncryptionResponse::read(
                            &mut payload,
                            &version,
                        )?,
                    )
                    .await)
            }
            id if id
                == pumpkin_protocol::java::server::login::SLoginPluginResponse::to_id(version) =>
            {
                Ok(self
                    .handle_plugin_response(
                        server,
                        pumpkin_protocol::java::server::login::SLoginPluginResponse::read(
                            &mut payload,
                            &version,
                        )?,
                    )
                    .await)
            }
            id if id
                == pumpkin_protocol::java::server::login::SLoginCookieResponse::to_id(version) =>
            {
                self.handle_login_cookie_response(
                    &pumpkin_protocol::java::server::login::SLoginCookieResponse::read(
                        &mut payload,
                        &version,
                    )?,
                );
                Ok(None)
            }
            id if id
                == pumpkin_protocol::java::server::login::SLoginAcknowledged::to_id(version) =>
            {
                Ok(self.handle_login_acknowledged(server).await)
            }
            _ => Err(ReadingError::Message(format!(
                "Failed to handle packet id {} in Login State",
                packet.id
            ))),
        }
    }

    async fn handle_config_packet(
        &mut self,
        server: &Arc<Server>,
        packet: &RawPacket,
    ) -> Result<Option<PacketHandlerResult>, ReadingError> {
        debug!("Handling config group");
        let mut payload = &packet.payload[..];
        let version = self.version.load();

        match packet.id {
            id if id == SClientInformationConfig::to_id(version) => {
                self.handle_client_information_config(SClientInformationConfig::read(
                    &mut payload,
                    &version,
                )?)
                .await;
                Ok(None)
            }
            id if id == SPluginMessage::to_id(version) => {
                self.handle_plugin_message(SPluginMessage::read(&mut payload, &version)?)
                    .await;
                Ok(None)
            }
            id if id == SAcknowledgeFinishConfig::to_id(version) => {
                if !payload.is_empty() {
                    return Err(ReadingError::Message(
                        "Trailing data in finish configuration acknowledgment".into(),
                    ));
                }
                if !self.configuration_phase.load().accepts_finish_ack() {
                    return Err(ReadingError::Message(
                        "Received finish configuration acknowledgment before finish configuration"
                            .into(),
                    ));
                }
                let Some(profile) = self.gameprofile.clone() else {
                    return Ok(Some(PacketHandlerResult::Stop));
                };
                let config = self.config.clone().unwrap_or_default();
                if let Some(reason) = can_not_join(&profile, &self.address, server).await {
                    self.kick(reason).await;
                    Ok(Some(PacketHandlerResult::Stop))
                } else {
                    self.configuration_phase.store(ConfigurationPhase::Play);
                    self.connection_state.store(ConnectionState::Play);
                    Ok(Some(PacketHandlerResult::ReadyToPlay(profile, config)))
                }
            }
            id if id == SKnownPacks::to_id(version) => {
                if !self.configuration_phase.load().accepts_known_packs() {
                    return Err(ReadingError::Message(
                        "Received known packs without a pending server request".into(),
                    ));
                }
                let known_packs = SKnownPacks::read(&mut payload, &version)?;
                if !payload.is_empty() {
                    return Err(ReadingError::Message(
                        "Trailing data in select known packs packet".into(),
                    ));
                }
                let selection = self.known_packs_selection(&known_packs.known_packs);
                self.handle_known_packs_with_selection(server, selection)
                    .await;
                Ok(None)
            }
            id if id == SConfigResourcePack::to_id(version) => {
                if !self.configuration_phase.load().accepts_resource_pack() {
                    return Err(ReadingError::Message(
                        "Received resource pack response without a pending resource pack".into(),
                    ));
                }
                let response = SConfigResourcePack::read(&mut payload, &version)?;
                if !payload.is_empty() {
                    return Err(ReadingError::Message(
                        "Trailing data in resource pack response".into(),
                    ));
                }
                self.handle_resource_pack_response(server, response).await;
                Ok(None)
            }
            id if id == SConfigCookieResponse::to_id(version) => {
                self.handle_config_cookie_response(&SConfigCookieResponse::read(
                    &mut payload,
                    &version,
                )?);
                Ok(None)
            }
            id if id == SConfigPong::to_id(version) => {
                let _pong = SConfigPong::read(&mut payload, &version)?;
                Ok(None)
            }
            id if id == SAcceptCodeOfConduct::to_id(version) => {
                let _accept = SAcceptCodeOfConduct::read(&mut payload, &version)?;
                Ok(None)
            }
            _ => Err(ReadingError::Message(format!(
                "Failed to handle packet id {} in Config State",
                packet.id
            ))),
        }
    }

    pub async fn handle_client_information_config(
        &mut self,
        client_information: SClientInformationConfig<'_>,
    ) {
        debug!("Handling client settings");
        if client_information.view_distance <= 0 {
            self.kick(TextComponent::text(
                "Cannot have zero or negative view distance!",
            ))
            .await;
            return;
        }

        if let (Ok(main_hand), Ok(chat_mode)) = (
            Hand::try_from(client_information.main_hand.0),
            ChatMode::try_from(client_information.chat_mode.0),
        ) {
            self.config = Some(PlayerConfig {
                locale: client_information.locale.to_string(),
                view_distance: NonZero::new(client_information.view_distance as u8)
                    .unwrap_or(NonZero::<u8>::MIN),
                chat_mode,
                chat_colors: client_information.chat_colors,
                skin_parts: client_information.skin_parts,
                main_hand,
                text_filtering: client_information.text_filtering,
                server_listing: client_information.server_listing,
            });
        } else {
            self.kick(TextComponent::text("Invalid hand or chat type"))
                .await;
        }
    }

    pub async fn handle_plugin_message(&mut self, plugin_message: SPluginMessage<'_>) {
        debug!("Handling plugin message");
        if plugin_message.channel.starts_with(BRAND_CHANNEL_PREFIX) {
            debug!("Got a client brand");
            match core::str::from_utf8(plugin_message.data) {
                Ok(brand) => self.brand = Some(brand.to_string()),
                Err(e) => self.kick(TextComponent::text(e.to_string())).await,
            }
        }
    }

    pub async fn handle_resource_pack_response(
        &mut self,
        server: &Server,
        packet: SConfigResourcePack,
    ) {
        let resource_config = &server.advanced_config.resource_pack.java;
        if !resource_config.enabled {
            self.kick(TextComponent::text(
                "Resource pack response is not expected",
            ))
            .await;
            return;
        }

        if !super::resource_pack_uuid_matches(&resource_config.url, packet.uuid) {
            warn!(
                "Client {} returned a response for an unknown resource pack",
                self.id
            );
            self.kick(TextComponent::text("Unknown resource pack response"))
                .await;
            return;
        }

        match super::resource_pack_response_action(&packet.response_result(), resource_config.force)
        {
            super::ResourcePackResponseAction::Wait => {}
            super::ResourcePackResponseAction::Complete => {
                if self.version.load() >= JavaMinecraftVersion::V_1_20_5 {
                    self.send_known_packs(server).await;
                } else {
                    self.handle_known_packs(server).await;
                }
            }
            super::ResourcePackResponseAction::Kick(reason) => {
                self.kick(TextComponent::text(reason)).await;
            }
        }
    }

    pub fn handle_config_cookie_response(&self, packet: &SConfigCookieResponse<'_>) {
        debug!(
            "Received cookie_response[config]: key: \"{}\", payload_length: \"{:?}\"",
            packet.key,
            packet.payload.as_ref().map(|p| p.len())
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigurationPhase, decode_status_packet};
    use pumpkin_protocol::java::packet_decoder::TCPNetworkDecoder;
    use pumpkin_util::version::JavaMinecraftVersion;

    #[tokio::test]
    async fn status_decoder_rejects_trailing_payload_after_protocol_decode() {
        let version = JavaMinecraftVersion::V_26_2;
        let mut ping_frame = vec![10, 1];
        ping_frame.extend_from_slice(&[0; 8]);
        ping_frame.push(0);

        for frame in [vec![2, 0, 0], ping_frame] {
            let mut decoder = TCPNetworkDecoder::new(frame.as_slice());
            let packet = decoder
                .get_raw_packet()
                .await
                .expect("status fixture frame should decode");
            assert!(
                decode_status_packet(&packet, version).is_err(),
                "status handler must reject trailing body bytes"
            );
        }
    }

    #[test]
    fn configuration_finish_ack_requires_finish_packet_to_be_sent() {
        assert!(!ConfigurationPhase::AwaitingKnownPacks.accepts_finish_ack());
        assert!(!ConfigurationPhase::AwaitingResourcePack.accepts_finish_ack());
        assert!(ConfigurationPhase::AwaitingFinishAck.accepts_finish_ack());
        assert!(!ConfigurationPhase::Play.accepts_finish_ack());
    }

    #[test]
    fn configuration_known_packs_requires_server_request_and_rejects_duplicates() {
        assert!(ConfigurationPhase::AwaitingKnownPacks.accepts_known_packs());
        assert!(!ConfigurationPhase::AwaitingFinishAck.accepts_known_packs());
        assert!(!ConfigurationPhase::Play.accepts_known_packs());
    }

    #[test]
    fn configuration_resource_pack_response_is_only_valid_while_pending() {
        assert!(ConfigurationPhase::AwaitingResourcePack.accepts_resource_pack());
        assert!(!ConfigurationPhase::AwaitingKnownPacks.accepts_resource_pack());
        assert!(!ConfigurationPhase::AwaitingFinishAck.accepts_resource_pack());
    }
}
