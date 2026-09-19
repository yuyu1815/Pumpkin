use pumpkin_protocol::java::client::{
    config::{CConfigDisconnect, CFinishConfig, CKeepAlive as CConfigKeepAlive},
    login::CLoginDisconnect,
    play::{CChunkBatchEnd, CChunkBatchStart, CLightUpdate, CPlayDisconnect, CStartConfiguration},
};
use pumpkin_world::level::SyncChunk;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::{
    collections::VecDeque,
    io::Write,
    sync::{Arc, Mutex},
};

use bytes::Bytes;
use crossbeam::atomic::AtomicCell;
use pumpkin_data::translation;
use pumpkin_protocol::java::server::config::{
    SAcceptCodeOfConduct, SAcknowledgeFinishConfig, SClientInformationConfig,
    SConfigCookieResponse, SConfigPong, SConfigResourcePack, SKeepAlive as SConfigKeepAlive,
    SKnownPacks, SPluginMessage as SConfigPluginMessage,
};
use pumpkin_protocol::java::server::play::{
    SAttack, SBlockEntityTagQuery, SBundleItemSelected, SChangeDifficulty, SChangeGameMode,
    SChatAck, SChatCommand, SChatCommandSigned, SChatMessage, SChunkBatch, SClickSlot,
    SClientCommand, SClientInformationPlay, SClientTickEnd, SCloseContainer, SCommandSuggestion,
    SConfigurationAcknowledged, SConfirmTeleport, SContainerButtonClick,
    SContainerSlotStateChanged, SCookieResponse as SPCookieResponse, SCustomPayload,
    SDebugSampleSubscription, SDebugSubscriptionRequest, SEditBook, SEntityTagQuery, SInteract,
    SJigsawGenerate, SLockDifficulty, SMoveVehicle, SPaddleBoat, SPickItemFromBlock, SPlaceRecipe,
    SPlayPingRequest, SPlayPong, SPlayResourcePack, SPlayerAbilities, SPlayerAction,
    SPlayerCommand, SPlayerInput, SPlayerLoaded, SPlayerPosition, SPlayerPositionRotation,
    SPlayerRotation, SPlayerSession, SRecipeBookChangeSettings, SRecipeBookSeenRecipe, SRenameItem,
    SSeenAdvancement, SSelectTrade, SSetBeacon, SSetCommandBlock, SSetCommandMinecart,
    SSetCreativeSlot, SSetGameRule, SSetHeldItem, SSetJigsawBlock, SSetPlayerGround,
    SSetStructureBlock, SSetTestBlock, SSpectateEntity, SSwingArm, STeleportToEntity,
    STestInstanceBlockAction, SUpdateSign, SUseItem, SUseItemOn,
};
use pumpkin_protocol::packet::MultiVersionJavaPacket;
use pumpkin_protocol::{
    ClientPacket, ConnectionState, MAX_PACKET_SIZE, PacketDecodeError, PacketEncodeError,
    RawPacket, ServerPacket,
    codec::var_int::VarInt,
    java::{packet_decoder::TCPNetworkDecoder, packet_encoder::TCPNetworkEncoder},
    ser::{NetworkWriteExt, ReadingError, WritingError},
};
use pumpkin_util::text::TextComponent;
use pumpkin_util::version::JavaMinecraftVersion;
use tokio::{
    io::{BufReader, BufWriter},
    net::tcp::{OwnedReadHalf, OwnedWriteHalf},
    sync::oneshot,
};
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, error, warn};

pub mod chunk_data;
pub mod config;
pub mod handshake;
pub mod login;
pub mod pending;
pub mod play;
pub mod recipe_helper;
pub mod status;

pub use chunk_data::{CChunkData, ChunkLightExt};

use arc_swap::ArcSwap;
use pending::PendingConnection;

use crate::entity::player::Player;
use crate::net::{
    ClientPlatform, GameProfile, MAX_PENDING_BYTES, PacketHandlerResult, PacketRateLimiter,
    PlayerConfig, decrement_pending_bytes,
};
use crate::plugin::api::events::world::chunk_send::ChunkSend;
use crate::plugin::player::player_custom_payload::PlayerCustomPayloadEvent;
use crate::{error::PumpkinError, server::Server};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum ConfigurationPhase {
    NotInConfiguration,
    AwaitingResourcePack,
    AwaitingKnownPacks,
    AwaitingFinishAck,
    Play,
    ReconfigurationAwaitingStart,
    ReconfigurationAwaitingAck,
}

impl ConfigurationPhase {
    pub(crate) const fn blocks_outgoing_play_packets(self) -> bool {
        matches!(
            self,
            Self::AwaitingResourcePack
                | Self::AwaitingKnownPacks
                | Self::AwaitingFinishAck
                | Self::ReconfigurationAwaitingStart
                | Self::ReconfigurationAwaitingAck
        )
    }

    pub(crate) const fn accepts_known_packs(self) -> bool {
        matches!(self, Self::AwaitingKnownPacks)
    }

    pub(crate) const fn accepts_finish_ack(self) -> bool {
        matches!(self, Self::AwaitingFinishAck)
    }

    pub(crate) const fn accepts_resource_pack(self) -> bool {
        matches!(self, Self::AwaitingResourcePack)
    }
}

fn claim_reconfiguration(phase: &AtomicCell<ConfigurationPhase>) -> bool {
    phase
        .compare_exchange(
            ConfigurationPhase::Play,
            ConfigurationPhase::ReconfigurationAwaitingStart,
        )
        .is_ok()
}

fn claim_reconfiguration_ack(phase: &AtomicCell<ConfigurationPhase>) -> bool {
    phase
        .compare_exchange(
            ConfigurationPhase::ReconfigurationAwaitingAck,
            ConfigurationPhase::AwaitingKnownPacks,
        )
        .is_ok()
}

fn claim_finish(phase: &AtomicCell<ConfigurationPhase>) -> bool {
    phase
        .compare_exchange(
            ConfigurationPhase::AwaitingFinishAck,
            ConfigurationPhase::Play,
        )
        .is_ok()
}

fn require_empty_body(payload: &[u8], packet_name: &str) -> Result<(), ReadingError> {
    if payload.is_empty() {
        Ok(())
    } else {
        Err(ReadingError::Message(format!(
            "Trailing data in {packet_name}"
        )))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KnownPackIdentity {
    namespace: String,
    id: String,
    version: String,
}

impl From<&pumpkin_protocol::KnownPack<'_>> for KnownPackIdentity {
    fn from(pack: &pumpkin_protocol::KnownPack<'_>) -> Self {
        Self {
            namespace: pack.namespace.to_owned(),
            id: pack.id.to_owned(),
            version: pack.version.to_owned(),
        }
    }
}

pub(crate) type KnownPacksState = Arc<Mutex<Option<Vec<KnownPackIdentity>>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KnownPacksSelection {
    Exact,
    Fallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourcePackResponseAction {
    Wait,
    Complete,
    Kick(&'static str),
}

pub(crate) const fn resource_pack_response_action(
    result: &pumpkin_protocol::java::server::config::ResourcePackResponseResult,
    force: bool,
) -> ResourcePackResponseAction {
    use pumpkin_protocol::java::server::config::ResourcePackResponseResult;

    match result {
        ResourcePackResponseResult::Accepted | ResourcePackResponseResult::Downloaded => {
            ResourcePackResponseAction::Wait
        }
        ResourcePackResponseResult::DownloadSuccess
        | ResourcePackResponseResult::DownloadFail
        | ResourcePackResponseResult::Discarded
        | ResourcePackResponseResult::Unknown(_) => ResourcePackResponseAction::Complete,
        ResourcePackResponseResult::Declined if force => {
            ResourcePackResponseAction::Kick("Required resource pack was declined")
        }
        ResourcePackResponseResult::Declined
        | ResourcePackResponseResult::InvalidUrl
        | ResourcePackResponseResult::ReloadFailed => ResourcePackResponseAction::Complete,
    }
}

pub(crate) fn resource_pack_uuid(url: &str) -> uuid::Uuid {
    uuid::Uuid::new_v3(&uuid::Uuid::NAMESPACE_DNS, url.as_bytes())
}

pub(crate) fn resource_pack_uuid_matches(url: &str, actual: uuid::Uuid) -> bool {
    resource_pack_uuid(url) == actual
}

fn known_packs_match(
    request: &[KnownPackIdentity],
    response: &[pumpkin_protocol::KnownPack<'_>],
) -> bool {
    request.len() == response.len()
        && request.iter().zip(response).all(|(expected, actual)| {
            expected.namespace == actual.namespace
                && expected.id == actual.id
                && expected.version == actual.version
        })
}

fn remember_known_packs(state: &KnownPacksState, request: &[pumpkin_protocol::KnownPack<'_>]) {
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *state = Some(request.iter().map(KnownPackIdentity::from).collect());
}

fn select_known_packs(
    state: &KnownPacksState,
    response: &[pumpkin_protocol::KnownPack<'_>],
) -> KnownPacksSelection {
    let state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state
        .as_deref()
        .is_some_and(|request| known_packs_match(request, response))
        .then_some(KnownPacksSelection::Exact)
        .unwrap_or(KnownPacksSelection::Fallback)
}

/// The generated registry table has no per-entry Known Pack ownership metadata.
/// Keep every registry packet and every entry payload for both official selection
/// outcomes; only a safe per-entry omission can be added once that metadata exists.
pub(crate) fn registry_entries_for_known_packs(
    registry: &pumpkin_data::registry::Registry,
    _selection: KnownPacksSelection,
) -> &[pumpkin_data::registry::RegistryEntryData] {
    &registry.registry_entries
}

pub struct JavaClient {
    pub id: u64,
    pub version: AtomicCell<JavaMinecraftVersion>,
    /// The client's game profile information. Direct field (lock-free).
    pub gameprofile: GameProfile,
    /// The client's configuration settings. Lock-free `ArcSwap`.
    pub config: ArcSwap<PlayerConfig>,
    /// The Address used to connect to the Server, Sent in the Handshake. Direct field.
    pub server_address: String,
    /// The current connection state of the client (e.g., Handshaking, Status, Play).
    pub connection_state: AtomicCell<ConnectionState>,
    /// The single source of truth for configuration sub-phases, shared with the pending connection.
    pub(crate) configuration_phase: AtomicCell<ConfigurationPhase>,
    /// The most recently sent Known Packs request, shared across initial and future config paths.
    pub(crate) known_packs_state: KnownPacksState,
    /// The client's IP address. Direct field.
    pub address: SocketAddr,
    /// The client's brand or modpack information. Lock-free `ArcSwap`.
    pub brand: ArcSwap<Option<String>>,
    /// Associated player reference. Lock-free `ArcSwap`.
    pub player: ArcSwap<Option<Arc<Player>>>,
    /// A collection of tasks associated with this client. The tasks await completion when removing the client.
    tasks: TaskTracker,
    rt_handle: tokio::runtime::Handle,
    /// An notifier that is triggered when this client is closed.
    close_token: CancellationToken,
    /// The single FIFO queue consumed by the sole TCP writer.
    outgoing_packet_send: UnboundedSender<OutgoingPacket>,
    outgoing_packet_recv: Option<UnboundedReceiver<OutgoingPacket>>,
    /// Serializes state capture, reconfiguration barriers, and deferred Play egress.
    egress_gate: Arc<Mutex<EgressGate>>,
    /// Monotonically identifies a Play/Configuration egress epoch.
    egress_epoch: Arc<AtomicU64>,
    /// Optional test-only writer boundary trace.
    egress_trace_file: Option<Arc<std::path::PathBuf>>,
    egress_trace_lock: Arc<Mutex<()>>,
    /// Tracks total buffered payload bytes in the outgoing queue and deferred packets.
    pub pending_bytes: Arc<AtomicUsize>,
    /// The packet encoder for outgoing packets.
    network_writer: std::sync::Mutex<Option<TCPNetworkEncoder<BufWriter<OwnedWriteHalf>>>>,
    /// The packet decoder for incoming packets.
    network_reader: std::sync::Mutex<Option<TCPNetworkDecoder<BufReader<OwnedReadHalf>>>>,
    /// Keep Alive:
    ///
    /// Whether we are waiting for a response after sending a keep alive packet.
    pub wait_for_keep_alive: AtomicBool,
    /// Set to `true` when any movement packet is received this tick.
    /// On `SClientTickEnd` (≥1.21.4), if still `false`, the player's known
    /// movement is zeroed (they stood still). Matches vanilla's `receivedMovementThisTick`.
    pub received_movement_this_tick: AtomicBool,
    /// The keep alive packet payload we send. The client should respond with the same id.
    pub keep_alive_id: AtomicCell<i64>,
    /// The last time we sent a keep alive packet.
    pub last_keep_alive_time: AtomicCell<Instant>,
    /// The last time any packet was received from the client.
    pub last_packet_time: AtomicCell<Instant>,
    /// Recent in-flight keep alive IDs with their sent timestamps.
    pub pending_keep_alives: std::sync::Mutex<Vec<(i64, Instant)>>,

    pub packet_sequence: AtomicI32,
    /// Packet rate limiter for incoming client packets.
    pub packet_limiter: PacketRateLimiter,
}

pub enum OutgoingPacketType {
    Normal,
    HighPriority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutgoingPacketKind {
    Data,
    StartConfiguration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutgoingPacketOrigin {
    Typed,
    Raw,
}

struct OutgoingPacket {
    data: Bytes,
    completion: Option<oneshot::Sender<()>>,
    version: JavaMinecraftVersion,
    connection_state: ConnectionState,
    configuration_phase: ConfigurationPhase,
    epoch: u64,
    packet_id: Option<i32>,
    kind: OutgoingPacketKind,
}

struct EgressGate {
    deferred_play: VecDeque<OutgoingPacket>,
}

fn serialized_packet_id(data: &[u8]) -> Option<i32> {
    let mut value = 0i32;
    let mut shift = 0;
    for byte in data.iter().copied().take(5) {
        value |= i32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

fn trace_egress(
    trace_file: &Option<Arc<std::path::PathBuf>>,
    trace_lock: &Mutex<()>,
    message: impl std::fmt::Display,
) {
    let Some(path) = trace_file else {
        return;
    };
    let _guard = trace_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path.as_ref())
    {
        use std::io::Write as _;
        let _ = writeln!(file, "{message}");
    }
}

impl OutgoingPacket {
    fn new(
        data: Bytes,
        completion: Option<oneshot::Sender<()>>,
        version: JavaMinecraftVersion,
        connection_state: ConnectionState,
        configuration_phase: ConfigurationPhase,
        epoch: u64,
        kind: OutgoingPacketKind,
    ) -> Self {
        Self {
            packet_id: serialized_packet_id(&data),
            data,
            completion,
            version,
            connection_state,
            configuration_phase,
            epoch,
            kind,
        }
    }
}

const MAX_FRAME_BATCH_DATA_SIZE: usize = MAX_PACKET_SIZE as usize;

fn take_frame_batch(packets: &mut VecDeque<OutgoingPacket>) -> Vec<OutgoingPacket> {
    let mut batch = Vec::new();
    let mut data_len = 0usize;

    while let Some(packet) = packets.pop_front() {
        let next_len = data_len.saturating_add(packet.data.len());
        if !batch.is_empty() && next_len > MAX_FRAME_BATCH_DATA_SIZE {
            packets.push_front(packet);
            break;
        }

        data_len = next_len;
        batch.push(packet);
    }

    batch
}

fn frame_packet_batch(
    mut writer: TCPNetworkEncoder<BufWriter<OwnedWriteHalf>>,
    batch: &[OutgoingPacket],
) -> (
    TCPNetworkEncoder<BufWriter<OwnedWriteHalf>>,
    Vec<u8>,
    Option<PacketEncodeError>,
) {
    let mut frame = Vec::new();
    let mut frame_err = None;
    for packet in batch {
        if let Err(err) = writer.frame_packet(&packet.data, &mut frame) {
            frame_err = Some(err);
            break;
        }
    }
    (writer, frame, frame_err)
}

async fn frame_batch_maybe_offload(
    writer: TCPNetworkEncoder<BufWriter<OwnedWriteHalf>>,
    packet_batch: Vec<OutgoingPacket>,
) -> Result<
    (
        TCPNetworkEncoder<BufWriter<OwnedWriteHalf>>,
        Vec<OutgoingPacket>,
        Vec<u8>,
        Option<PacketEncodeError>,
    ),
    tokio::task::JoinError,
> {
    let needs_offload = packet_batch
        .iter()
        .any(|packet| writer.is_compressing_packet(&packet.data));

    if needs_offload {
        tokio::task::spawn_blocking(move || {
            let (writer, frame, frame_err) = frame_packet_batch(writer, &packet_batch);
            (writer, packet_batch, frame, frame_err)
        })
        .await
    } else {
        let (writer, frame, frame_err) = frame_packet_batch(writer, &packet_batch);
        Ok((writer, packet_batch, frame, frame_err))
    }
}

impl JavaClient {
    pub(crate) fn remember_known_packs(&self, request: &[pumpkin_protocol::KnownPack<'_>]) {
        remember_known_packs(&self.known_packs_state, request);
    }

    pub(crate) fn known_packs_selection(
        &self,
        response: &[pumpkin_protocol::KnownPack<'_>],
    ) -> KnownPacksSelection {
        select_known_packs(&self.known_packs_state, response)
    }

    fn enqueue_encoded(
        &self,
        data: Bytes,
        completion: Option<oneshot::Sender<()>>,
        forced_phase: Option<ConfigurationPhase>,
        kind: OutgoingPacketKind,
        origin: OutgoingPacketOrigin,
        force_main_queue: bool,
    ) -> bool {
        if self.close_token.is_cancelled() {
            return false;
        }

        let packet_len = data.len();
        let mut gate = self
            .egress_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = self.connection_state.load();
        let encoded_state =
            if origin == OutgoingPacketOrigin::Raw && state == ConnectionState::Config {
                // Raw bytes from world/entity/chunk broadcasters are Play packets. Typed
                // configuration sends use the explicit Typed path below; never infer a
                // protocol state from the mutable connection state alone.
                ConnectionState::Play
            } else {
                state
            };
        let phase = forced_phase.unwrap_or_else(|| self.configuration_phase.load());
        let epoch = self.egress_epoch.load(Ordering::Acquire);
        let version = self.version.load();
        let packet =
            OutgoingPacket::new(data, completion, version, encoded_state, phase, epoch, kind);
        let new_bytes = self
            .pending_bytes
            .fetch_add(packet_len, Ordering::AcqRel)
            .saturating_add(packet_len);

        if new_bytes > MAX_PENDING_BYTES {
            decrement_pending_bytes(&self.pending_bytes, packet_len);
            drop(gate);
            if !self.close_token.is_cancelled() {
                warn!(
                    "Client {} outbound packet buffer overflow ({} bytes > {} bytes). Closing connection.",
                    self.id, new_bytes, MAX_PENDING_BYTES
                );
                self.close();
            }
            return false;
        }

        let deferred = !force_main_queue
            && encoded_state == ConnectionState::Play
            && phase.blocks_outgoing_play_packets();
        trace_egress(
            &self.egress_trace_file,
            &self.egress_trace_lock,
            format_args!(
                "enqueue epoch={} actual_state={state:?} encoded_state={encoded_state:?} phase={phase:?} id={:?} len={} origin={origin:?} deferred={deferred}",
                epoch, packet.packet_id, packet_len
            ),
        );
        if deferred {
            gate.deferred_play.push_back(packet);
            return true;
        }

        if self.outgoing_packet_send.send(packet).is_err() {
            decrement_pending_bytes(&self.pending_bytes, packet_len);
            drop(gate);
            if !self.close_token.is_cancelled() {
                self.close();
            }
            return false;
        }
        true
    }

    fn enqueue_encoded_wait(
        &self,
        data: Bytes,
        forced_phase: Option<ConfigurationPhase>,
        kind: OutgoingPacketKind,
        origin: OutgoingPacketOrigin,
        force_main_queue: bool,
    ) -> Option<oneshot::Receiver<()>> {
        let (completion_tx, completion_rx) = oneshot::channel();
        if !self.enqueue_encoded(
            data,
            Some(completion_tx),
            forced_phase,
            kind,
            origin,
            force_main_queue,
        ) {
            return None;
        }
        Some(completion_rx)
    }

    /// Starts one Play -> Configuration transition.
    ///
    /// The egress gate changes epoch and appends the marker to the one FIFO
    /// writer queue while holding the same lock used by every sender. Packets
    /// already captured as Play therefore drain first; later Play packets are
    /// held with their state/epoch metadata until the Finish acknowledgment.
    pub async fn start_reconfiguration(&self) -> bool {
        if !self.version.load().supports_configuration_state()
            || self.connection_state.load() != ConnectionState::Play
        {
            return false;
        }

        let data = match self.serialize_packet(&CStartConfiguration) {
            Ok(data) => data,
            Err(_) => return false,
        };
        let (completion_tx, completion_rx) = oneshot::channel();
        {
            let _gate = self
                .egress_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !claim_reconfiguration(&self.configuration_phase) {
                return false;
            }
            let epoch = self.egress_epoch.fetch_add(1, Ordering::AcqRel) + 1;
            let version = self.version.load();
            let packet = OutgoingPacket::new(
                data,
                Some(completion_tx),
                version,
                ConnectionState::Play,
                ConfigurationPhase::ReconfigurationAwaitingStart,
                epoch,
                OutgoingPacketKind::StartConfiguration,
            );
            self.configuration_phase
                .store(ConfigurationPhase::ReconfigurationAwaitingAck);
            let packet_len = packet.data.len();
            self.pending_bytes.fetch_add(packet_len, Ordering::AcqRel);
            trace_egress(
                &self.egress_trace_file,
                &self.egress_trace_lock,
                format_args!(
                    "enqueue-marker epoch={} state=Play phase=ReconfigurationAwaitingStart id={:?} len={}",
                    epoch, packet.packet_id, packet_len
                ),
            );
            if self.outgoing_packet_send.send(packet).is_err() {
                decrement_pending_bytes(&self.pending_bytes, packet_len);
                drop(_gate);
                self.close();
                return false;
            }
        }

        if completion_rx.await.is_err() {
            trace_egress(
                &self.egress_trace_file,
                &self.egress_trace_lock,
                "start-marker-completion-error",
            );
            return false;
        }
        true
    }

    pub(crate) fn complete_reconfiguration_egress(&self) -> bool {
        let mut gate = self
            .egress_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !claim_finish(&self.configuration_phase) {
            return false;
        }
        self.connection_state.store(ConnectionState::Play);
        while let Some(packet) = gate.deferred_play.pop_front() {
            trace_egress(
                &self.egress_trace_file,
                &self.egress_trace_lock,
                format_args!(
                    "release-deferred epoch={} captured_state={:?} captured_phase={:?} id={:?} len={}",
                    packet.epoch,
                    packet.connection_state,
                    packet.configuration_phase,
                    packet.packet_id,
                    packet.data.len()
                ),
            );
            if self.outgoing_packet_send.send(packet).is_err() {
                drop(gate);
                self.close();
                return false;
            }
        }
        true
    }

    /// Dispatches serverbound Configuration packets after a reconfiguration ack.
    pub(crate) async fn handle_configuration_packet(
        &self,
        server: &Server,
        packet: &RawPacket,
    ) -> Result<(), ReadingError> {
        let mut payload = &packet.payload[..];
        let version = self.version.load();

        match packet.id {
            id if id == SClientInformationConfig::to_id(version) => {
                let client_information = SClientInformationConfig::read(&mut payload, &version)?;
                if !payload.is_empty() {
                    return Err(ReadingError::Message(
                        "Trailing data in client information packet".into(),
                    ));
                }
                self.handle_client_information_config(client_information)
                    .await;
            }
            id if id == SConfigPluginMessage::to_id(version) => {
                let plugin_message = SConfigPluginMessage::read(&mut payload, &version)?;
                if !payload.is_empty() {
                    return Err(ReadingError::Message(
                        "Trailing data in configuration plugin message".into(),
                    ));
                }
                self.handle_plugin_message(plugin_message).await;
            }
            id if id == SConfigKeepAlive::to_id(version) => {
                let keep_alive = SConfigKeepAlive::read(&mut payload, &version)?;
                if !payload.is_empty() {
                    return Err(ReadingError::Message(
                        "Trailing data in configuration keep alive".into(),
                    ));
                }
                self.handle_config_keep_alive(&keep_alive);
            }
            id if id == SConfigCookieResponse::to_id(version) => {
                let cookie = SConfigCookieResponse::read(&mut payload, &version)?;
                if !payload.is_empty() {
                    return Err(ReadingError::Message(
                        "Trailing data in configuration cookie response".into(),
                    ));
                }
                self.handle_config_cookie_response(&cookie);
            }
            id if id == SConfigPong::to_id(version) => {
                let _ = SConfigPong::read(&mut payload, &version)?;
                if !payload.is_empty() {
                    return Err(ReadingError::Message(
                        "Trailing data in configuration pong".into(),
                    ));
                }
            }
            id if id == SAcceptCodeOfConduct::to_id(version) => {
                let _ = SAcceptCodeOfConduct::read(&mut payload, &version)?;
                if !payload.is_empty() {
                    return Err(ReadingError::Message(
                        "Trailing data in code of conduct acceptance".into(),
                    ));
                }
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
                self.handle_known_packs_response(server, &known_packs).await;
            }
            id if id == SAcknowledgeFinishConfig::to_id(version) => {
                require_empty_body(&payload, "finish configuration acknowledgment")?;
                if !self.handle_reconfiguration_finish(server).await {
                    return Err(ReadingError::Message(
                        "Finish configuration acknowledgment is not pending".into(),
                    ));
                }
            }
            _ => {
                return Err(ReadingError::Message(format!(
                    "Failed to handle java client packet id {} in Configuration State",
                    packet.id
                )));
            }
        }

        if !payload.is_empty() {
            return Err(ReadingError::Message(
                "Trailing data in configuration packet".into(),
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn from_pending(
        pending: PendingConnection,
        gameprofile: GameProfile,
        config: PlayerConfig,
    ) -> Self {
        let (send, recv) = tokio::sync::mpsc::unbounded_channel();
        let trace_file = std::env::var_os("PUMPKIN_JAVA_EGRESS_TRACE_FILE")
            .map(std::path::PathBuf::from)
            .map(Arc::new);

        Self {
            id: pending.id,
            gameprofile,
            config: ArcSwap::from_pointee(config),
            server_address: pending.server_address,
            address: pending.address,
            connection_state: pending.connection_state,
            configuration_phase: pending.configuration_phase,
            known_packs_state: pending.known_packs_state,
            close_token: pending.close_token,
            tasks: TaskTracker::new(),
            rt_handle: tokio::runtime::Handle::current(),
            outgoing_packet_send: send,
            outgoing_packet_recv: Some(recv),
            egress_gate: Arc::new(Mutex::new(EgressGate {
                deferred_play: VecDeque::new(),
            })),
            egress_epoch: Arc::new(AtomicU64::new(0)),
            egress_trace_file: trace_file,
            egress_trace_lock: Arc::new(Mutex::new(())),
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            version: pending.version,
            network_writer: std::sync::Mutex::new(Some(pending.network_writer)),
            network_reader: std::sync::Mutex::new(Some(pending.network_reader)),
            brand: ArcSwap::from_pointee(pending.brand),
            player: ArcSwap::from_pointee(None),
            wait_for_keep_alive: AtomicBool::new(false),
            received_movement_this_tick: AtomicBool::new(false),
            keep_alive_id: AtomicCell::new(0),
            last_keep_alive_time: AtomicCell::new(Instant::now()),
            last_packet_time: AtomicCell::new(Instant::now()),
            pending_keep_alives: std::sync::Mutex::new(Vec::new()),
            packet_sequence: AtomicI32::new(-1),
            packet_limiter: pending.packet_limiter,
        }
    }

    pub fn set_player(&self, player: Arc<Player>) {
        self.player.store(Arc::new(Some(player)));
    }

    pub async fn progress_player_packets(&self, player: &Arc<Player>, server: &Arc<Server>) {
        let Some(mut network_reader) = self
            .network_reader
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };

        let keep_alive_time = server.advanced_config.networking.java.keep_alive_time;
        let mut keep_alive_interval =
            tokio::time::interval(std::time::Duration::from_secs(keep_alive_time.max(1)));
        let timeout_duration =
            std::time::Duration::from_secs(keep_alive_time.saturating_mul(2).max(1));

        // Skip the immediate first tick so we don't send a keep-alive the exact millisecond they join
        keep_alive_interval.tick().await;

        loop {
            tokio::select! {
                // KEEP-ALIVE TIMER
                _ = keep_alive_interval.tick() => {
                    // Check if the client has timed out on keep-alive responses or no packet activity
                    let has_timed_out = {
                        let pending = self
                            .pending_keep_alives
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        pending.iter().any(|(_, send_time)| send_time.elapsed() > timeout_duration)
                    } || (self.wait_for_keep_alive.load(Ordering::Relaxed) && self.last_keep_alive_time.load().elapsed() > timeout_duration)
                      || (self.last_packet_time.load().elapsed() > timeout_duration);

                    if has_timed_out {
                        self.kick(pumpkin_macros::translate_cross!(translation::java::DISCONNECT_TIMEOUT, translation::bedrock::DISCONNECT_TIMEOUT)).await;
                        break;
                    }

                    let keep_alive_id = i64::from(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as i32,
                    );

                    self.keep_alive_id.store(keep_alive_id);
                    self.wait_for_keep_alive.store(true, Ordering::Relaxed);
                    self.last_keep_alive_time.store(Instant::now());
                    {
                        let mut pending = self
                            .pending_keep_alives
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        pending.push((keep_alive_id, Instant::now()));
                        if pending.len() > 16 {
                            pending.remove(0);
                        }
                    }
                    if self.connection_state.load() == ConnectionState::Config {
                        self.enqueue_client_packet(&CConfigKeepAlive::new(keep_alive_id))
                            .await;
                    } else {
                        let packet =
                            pumpkin_protocol::java::client::play::CKeepAlive::new(keep_alive_id);
                        self.enqueue_client_packet(&packet).await;
                    }
                }

                () = self.close_token.cancelled() => {
                    break;
                }

                // INCOMING PACKETS
                packet_opt = self.get_packet_with_reader(&mut network_reader) => {
                    let Some(packet) = packet_opt else {
                        break;
                    };
                    self.last_packet_time.store(Instant::now());

                    if !self.packet_limiter.check_packet() {
                        warn!(
                            "Client {} ({}) exceeded packet rate limit (rate: {}/s)",
                            self.id,
                            self.gameprofile.name,
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
                        break;
                    }

                    if self.connection_state.load() == ConnectionState::Config {
                        if let Err(error) = self.handle_configuration_packet(server, &packet).await {
                            let text = format!("Error while reading configuration packet {error}");
                            self.kick(TextComponent::text(text)).await;
                            break;
                        }
                        continue;
                    }

                    if self.connection_state.load() == ConnectionState::Play
                        && packet.id
                            == SConfigurationAcknowledged::to_id(self.version.load())
                    {
                        let mut payload = &packet.payload[..];
                        if SConfigurationAcknowledged::read(&mut payload, &self.version.load())
                            .is_err()
                            || require_empty_body(&payload, "configuration acknowledgment").is_err()
                        {
                            self.kick(TextComponent::text(
                                "Invalid configuration acknowledgment body",
                            ))
                            .await;
                            break;
                        }
                        if self.handle_configuration_acknowledged(player) {
                            self.send_known_packs(server).await;
                        }
                        continue;
                    }

                    player.inbound_packets.push(packet);
                }
            }
        }
    }

    pub async fn await_tasks(&self) {
        self.tasks.close();
        self.tasks.wait().await;
    }

    /// Spawns a task associated with this client. All tasks spawned with this method are awaited
    /// when the client. This means tasks should complete in a reasonable amount of time or select
    /// on `Self::await_close_interrupt` to cancel the task when the client is closed
    ///
    /// Returns an `Option<JoinHandle<F::Output>>`. If the client is closed, this returns `None`.
    pub fn spawn_task<F>(&self, task: F) -> Option<JoinHandle<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if self.close_token.is_cancelled() {
            None
        } else {
            let _guard = self.rt_handle.enter();
            Some(self.tasks.spawn(task))
        }
    }

    pub async fn send_chunks(&self, chunks: &[SyncChunk]) {
        let player = self.player.load_full();
        let Some(player) = player.as_ref() else {
            return;
        };
        let Some(server) = player.world().server.upgrade() else {
            return;
        };

        let mut valid_chunks = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let mut event = ChunkSend::new(player.world(), chunk.clone());
            server.plugin_manager.fire(&server, &mut event).await;
            if !event.cancelled {
                valid_chunks.push(chunk.clone());
            }
        }

        if valid_chunks.is_empty() {
            return;
        }

        let version = self.version.load();
        let (tx, rx) = oneshot::channel();
        rayon::spawn(move || {
            let mut serialized = Vec::with_capacity(valid_chunks.len());
            for chunk in valid_chunks {
                let mut buf = Vec::with_capacity(32 * 1024);
                if let Err(err) = buf.write_var_int(&VarInt(CChunkData::to_id(version))) {
                    error!("Failed to write chunk data id: {err:?}");
                    continue;
                }
                if let Err(err) = CChunkData(&chunk).write_packet_data(&mut buf, &version) {
                    error!("Failed to write chunk data: {err:?}");
                    continue;
                }

                let light_buf = if version >= JavaMinecraftVersion::V_1_14
                    && version < JavaMinecraftVersion::V_1_18
                {
                    match CLightUpdate::from_chunk(&chunk, version) {
                        Ok(light_packet) => {
                            let mut light_buf = Vec::new();
                            if let Err(err) =
                                light_buf.write_var_int(&VarInt(CLightUpdate::to_id(version)))
                            {
                                error!("Failed to write light update id: {err:?}");
                                None
                            } else if let Err(err) =
                                light_packet.write_packet_data(&mut light_buf, &version)
                            {
                                error!("Failed to write light update data: {err:?}");
                                None
                            } else {
                                Some(Bytes::from(light_buf))
                            }
                        }
                        Err(err) => {
                            error!("Failed to create light update packet: {err:?}");
                            None
                        }
                    }
                } else {
                    None
                };

                serialized.push((Bytes::from(buf), light_buf));
            }
            let _ = tx.send(serialized);
        });

        let Ok(serialized) = rx.await else {
            return;
        };
        let sent_count = serialized.len();
        if sent_count == 0 {
            return;
        }

        if version >= JavaMinecraftVersion::V_1_20_2 {
            self.send_packet(&CChunkBatchStart).await;
        }

        // Keep the whole batch on the priority queue. Otherwise the batch end can overtake chunk
        // data queued on the normal channel, leaving the client unable to render those chunks.
        for (chunk_data, light_data) in serialized {
            self.send_packet_now_data(chunk_data).await;
            if let Some(light_data) = light_data {
                self.send_packet_now_data(light_data).await;
            }
        }

        if version >= JavaMinecraftVersion::V_1_20_2 {
            self.send_packet(&CChunkBatchEnd::new(sent_count as u16))
                .await;
        }
    }

    #[allow(clippy::unused_async)]
    pub async fn enqueue_packet(&self, packet_data: Bytes) {
        self.try_enqueue_packet_data(packet_data);
    }

    #[allow(clippy::unused_async)]
    pub async fn enqueue_packet_data(&self, packet_data: Bytes) {
        self.try_enqueue_packet_data(packet_data);
    }

    pub fn try_enqueue_packet(&self, packet_data: Bytes) {
        self.try_enqueue_packet_data(packet_data);
    }

    pub fn try_enqueue_packet_data(&self, packet_data: Bytes) {
        let _ = self.enqueue_encoded(
            packet_data,
            None,
            None,
            OutgoingPacketKind::Data,
            OutgoingPacketOrigin::Raw,
            false,
        );
    }

    pub async fn await_close_interrupt(&self) {
        self.close_token.cancelled().await;
    }

    pub async fn get_packet_with_reader(
        &self,
        network_reader: &mut TCPNetworkDecoder<BufReader<OwnedReadHalf>>,
    ) -> Option<RawPacket> {
        tokio::select! {
            () = self.await_close_interrupt() => {
                debug!("Canceling player packet processing");
                None
            },
            packet_result = network_reader.get_raw_packet() => {
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
        }
    }

    pub fn try_kick(&self, reason: &TextComponent) {
        let serialized = match self.connection_state.load() {
            ConnectionState::Login => {
                let packet = CLoginDisconnect::new(
                    serde_json::to_string(&reason.0).unwrap_or_else(|_| String::new()),
                );
                self.serialize_packet(&packet).ok()
            }
            ConnectionState::Config => {
                let reason_text = reason.clone().get_text();
                let packet = CConfigDisconnect::new(&reason_text);
                self.serialize_packet(&packet).ok()
            }
            ConnectionState::Play => {
                let packet = CPlayDisconnect::new(reason);
                self.serialize_packet(&packet).ok()
            }
            _ => None,
        };

        if let Some(data) = serialized {
            let _ = self.enqueue_encoded(
                data,
                None,
                None,
                OutgoingPacketKind::Data,
                OutgoingPacketOrigin::Typed,
                true,
            );
        }
        let reason_text = reason.clone().get_text();
        warn!("Closing connection for {}: {reason_text}", self.id);
        self.close();
    }

    pub async fn kick(&self, reason: TextComponent) {
        self.kick_explicit(&reason, true).await;
    }

    pub async fn kick_explicit(&self, reason: &TextComponent, send_packet: bool) {
        if send_packet {
            match self.connection_state.load() {
                ConnectionState::Login => {
                    // TextComponent implements Serialize and writes in bytes instead of String, that's the reason we only use content
                    self.send_packet(&CLoginDisconnect::new(
                        serde_json::to_string(&reason.0).unwrap_or_else(|_| String::new()),
                    ))
                    .await;
                }
                ConnectionState::Config => {
                    self.send_packet(&CConfigDisconnect::new(&reason.clone().get_text()))
                        .await;
                }
                ConnectionState::Play => self.send_packet(&CPlayDisconnect::new(reason)).await,
                _ => {}
            }
        }
        let reason_text = reason.clone().get_text();
        warn!("Closing connection for {}: {reason_text}", self.id);
        self.close();
    }

    pub async fn send_packet_now(&self, packet: Bytes) {
        self.send_packet_now_data(packet).await;
    }

    pub async fn send_packet_now_data(&self, packet: Bytes) {
        self.send_packet_now_data_inner(packet, OutgoingPacketOrigin::Raw)
            .await;
    }

    pub(crate) async fn send_packet_now_data_typed(&self, packet: Bytes) {
        self.send_packet_now_data_inner(packet, OutgoingPacketOrigin::Typed)
            .await;
    }

    async fn send_packet_now_data_inner(&self, packet: Bytes, origin: OutgoingPacketOrigin) {
        let Some(completion_rx) =
            self.enqueue_encoded_wait(packet, None, OutgoingPacketKind::Data, origin, false)
        else {
            return;
        };
        if completion_rx.await.is_err() && !self.close_token.is_cancelled() {
            self.close();
        }
    }

    pub fn write_packet_for_version<P: ClientPacket>(
        packet: &P,
        version: JavaMinecraftVersion,
        write: impl Write,
    ) -> Result<(), WritingError> {
        pumpkin_protocol::java::packet_encoder::write_packet(packet, &version, write)
    }

    pub fn serialize_packet_for_version<P: ClientPacket>(
        packet: &P,
        version: JavaMinecraftVersion,
    ) -> Result<Bytes, WritingError> {
        pumpkin_protocol::java::packet_encoder::serialize_packet(packet, &version)
    }

    pub fn serialize_packet<P: ClientPacket>(&self, packet: &P) -> Result<Bytes, WritingError> {
        Self::serialize_packet_for_version(packet, self.version.load())
    }

    pub fn try_send_packet<P: ClientPacket>(&self, packet: &P) {
        if let Ok(data) = self.serialize_packet(packet) {
            let _ = self.enqueue_encoded(
                data,
                None,
                None,
                OutgoingPacketKind::Data,
                OutgoingPacketOrigin::Typed,
                false,
            );
        }
    }

    pub async fn send_packet<P: ClientPacket>(&self, packet: &P) {
        if let Ok(data) = self.serialize_packet(packet) {
            self.send_packet_now_data_inner(data, OutgoingPacketOrigin::Typed)
                .await;
        }
    }

    pub async fn enqueue_client_packet<P: ClientPacket>(&self, packet: &P) {
        if let Ok(data) = self.serialize_packet(packet) {
            let _ = self.enqueue_encoded(
                data,
                None,
                None,
                OutgoingPacketKind::Data,
                OutgoingPacketOrigin::Typed,
                false,
            );
        }
    }

    pub fn write_packet<P: ClientPacket>(
        &self,
        packet: &P,
        write: impl Write,
    ) -> Result<(), WritingError> {
        Self::write_packet_for_version(packet, self.version.load(), write)
    }

    /// Handles an incoming packet, routing it to the appropriate handler based on the current connection state.
    ///
    /// This function takes a `RawPacket` and routes it to the corresponding handler based on the current connection state.
    /// It supports the following connection states:
    ///
    /// - **Handshake:** Handles handshake packets.
    /// - **Status:** Handles status request and ping packets.
    /// - **Login/Transfer:** Handles login and transfer packets.
    /// - **Config:** Handles configuration packets.
    #[expect(clippy::too_many_lines)]
    pub fn start_outgoing_packet_task(&mut self) {
        const MAX_BATCH_SIZE: usize = 64;

        let Some(mut packet_receiver) = self.outgoing_packet_recv.take() else {
            return;
        };
        let close_token = self.close_token.clone();
        let pending_bytes = self.pending_bytes.clone();
        let trace_file = self.egress_trace_file.clone();
        let trace_lock = self.egress_trace_lock.clone();
        let writer_version = self.version.load();
        let Some(mut writer) = self
            .network_writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let id = self.id;
        self.spawn_task(async move {
            trace_egress(
                &trace_file,
                &trace_lock,
                format_args!("writer-task-start version={writer_version:?}"),
            );
            let mut writer_epoch = 0u64;
            let mut reconfiguration_started = false;
            let mut finish_sent = false;
            loop {
                let recv_result = tokio::select! {
                    res = packet_receiver.recv() => res,
                    () = close_token.cancelled() => packet_receiver.try_recv().ok(),
                };

                let Some(packet_data) = recv_result else {
                    trace_egress(&trace_file, &trace_lock, "writer-task-exit receiver-closed");
                    break;
                };
                trace_egress(
                    &trace_file,
                    &trace_lock,
                    format_args!("writer-recv id={:?} epoch={}", packet_data.packet_id, packet_data.epoch),
                );

                let mut packet_batch = Vec::with_capacity(MAX_BATCH_SIZE);
                packet_batch.push(packet_data);

                while packet_batch.len() < MAX_BATCH_SIZE {
                    match packet_receiver.try_recv() {
                        Ok(packet_data) => packet_batch.push(packet_data),
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected
                            | tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    }
                }

                let mut boundary_violation = None;
                for packet in &packet_batch {
                    let start_id = CStartConfiguration::to_id(packet.version);
                    let finish_id = CFinishConfig::to_id(packet.version);
                    let valid = if packet.version != writer_version {
                        false
                    } else if packet.kind == OutgoingPacketKind::StartConfiguration {
                        let valid = packet.connection_state == ConnectionState::Play
                            && packet.configuration_phase
                                == ConfigurationPhase::ReconfigurationAwaitingStart
                            && packet.packet_id == Some(start_id)
                            && packet.epoch > writer_epoch;
                        if valid {
                            writer_epoch = packet.epoch;
                            reconfiguration_started = true;
                            finish_sent = false;
                        }
                        valid
                    } else if packet.epoch < writer_epoch {
                        false
                    } else if packet.connection_state == ConnectionState::Play
                        && packet.epoch == writer_epoch
                        && packet.configuration_phase == ConfigurationPhase::Play
                        && reconfiguration_started
                        && !finish_sent
                    {
                        false
                    } else {
                        if packet.packet_id == Some(finish_id) {
                            finish_sent = true;
                        }
                        true
                    };
                    trace_egress(
                        &trace_file,
                        &trace_lock,
                        format_args!(
                            "writer epoch={} captured_epoch={} state={:?} phase={:?} id={:?} len={} kind={:?} valid={valid}",
                            writer_epoch,
                            packet.epoch,
                            packet.connection_state,
                            packet.configuration_phase,
                            packet.packet_id,
                            packet.data.len(),
                            packet.kind
                        ),
                    );
                    if !valid && boundary_violation.is_none() {
                        boundary_violation = Some(format!(
                            "egress boundary violation: writer_epoch={} captured_epoch={} state={:?} phase={:?} id={:?} kind={:?}",
                            writer_epoch,
                            packet.epoch,
                            packet.connection_state,
                            packet.configuration_phase,
                            packet.packet_id,
                            packet.kind
                        ));
                    }
                }
                if let Some(reason) = boundary_violation {
                    trace_egress(&trace_file, &trace_lock, format_args!("FIRST {reason}"));
                    let dropped_bytes: usize = packet_batch.iter().map(|p| p.data.len()).sum();
                    decrement_pending_bytes(&pending_bytes, dropped_bytes);
                    for packet in packet_batch {
                        if let Some(completion) = packet.completion {
                            let _ = completion.send(());
                        }
                    }
                    close_token.cancel();
                    return;
                }

                let mut packets_to_frame = VecDeque::from(packet_batch);
                let mut written_packets = Vec::with_capacity(packets_to_frame.len());
                let mut send_failed = false;

                while !packets_to_frame.is_empty() {
                    let frame_batch = take_frame_batch(&mut packets_to_frame);
                    let (returned_writer, returned_batch, frame, frame_err) =
                        match frame_batch_maybe_offload(writer, frame_batch).await {
                            Ok(result) => result,
                            Err(err) => {
                                if !close_token.is_cancelled() {
                                    warn!("Packet framing task failed for client {id}: {err}");
                                }
                                close_token.cancel();
                                return;
                            }
                        };
                    writer = returned_writer;

                    if let Some(err) = frame_err {
                        if !close_token.is_cancelled() {
                            warn!("Failed to frame packet for client {id}: {err}");
                        }
                        send_failed = true;
                        break;
                    }

                    if let Err(err) = writer.write_frame(&frame).await {
                        if !close_token.is_cancelled() {
                            warn!("Failed to send packet batch to client {id}: {err}");
                        }
                        send_failed = true;
                        break;
                    }

                    written_packets.extend(returned_batch);
                }

                if !send_failed && let Err(err) = writer.flush().await {
                    if !close_token.is_cancelled() {
                        warn!("Failed to flush packet batch for client {id}: {err}");
                    }
                    send_failed = true;
                }

                let flushed_bytes: usize = written_packets.iter().map(|p| p.data.len()).sum();
                decrement_pending_bytes(&pending_bytes, flushed_bytes);

                if send_failed {
                    // We now need to close the connection to the client since the stream is in an unknown state.
                    close_token.cancel();
                    break;
                }

                for packet in written_packets {
                    if let Some(completion) = packet.completion {
                        let _ = completion.send(());
                    }
                }
            }
        });
    }

    /// Closes the connection to the client.
    ///
    /// This function marks the connection as closed using an atomic flag. It's generally preferable
    /// to use the `kick` function if you want to send a specific message to the client explaining the reason for the closure.
    /// However, use `close` in scenarios where sending a message is not critical or might not be possible (e.g., sudden connection drop).
    ///
    /// # Notes
    ///
    /// This function does not attempt to send any disconnect packets to the client.
    pub fn close(&self) {
        self.close_token.cancel();
        let mut gate = self
            .egress_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while let Some(packet) = gate.deferred_play.pop_front() {
            decrement_pending_bytes(&self.pending_bytes, packet.data.len());
            if let Some(completion) = packet.completion {
                let _ = completion.send(());
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.close_token.is_cancelled()
    }

    #[expect(clippy::too_many_lines)]
    pub fn handle_play_packet(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        packet: &RawPacket,
    ) -> Result<(), Box<dyn PumpkinError>> {
        let version = self.version.load();

        let mut event = crate::plugin::server::packet::PacketReceivedEvent::new(
            player.clone(),
            packet.id,
            packet.payload.clone(),
        );
        server.plugin_manager.fire_blocking(server, &mut event);
        if event.cancelled {
            return Ok(());
        }

        let mut payload = &event.payload[..];
        match event.packet_id {
            id if id == SConfirmTeleport::to_id(version) => {
                self.handle_confirm_teleport(
                    player,
                    &SConfirmTeleport::read(&mut payload, &version)?,
                );
            }
            id if id == SChangeGameMode::to_id(version) => {
                self.handle_change_game_mode(
                    player,
                    &SChangeGameMode::read(&mut payload, &version)?,
                );
            }
            id if id == SChatAck::to_id(version) => {
                let packet = SChatAck::read(&mut payload, &version)?;
                self.handle_chat_ack(player, &packet);
            }
            id if id == SChatCommand::to_id(version) => {
                let packet = SChatCommand::read(&mut payload, &version)?;
                let cmd = packet.command.to_string();
                let client_platform = player.client.clone();
                let player_c = player.clone();
                let server_c = server.clone();
                server.spawn_task(async move {
                    if let ClientPlatform::Java(client) = client_platform.as_ref() {
                        let packet = SChatCommand { command: &cmd };
                        client
                            .handle_chat_command(&player_c, &server_c, &packet)
                            .await;
                    }
                });
            }
            id if id == SChatCommandSigned::to_id(version) => {
                let mut signed_payload = payload;
                let cmd =
                    if let Ok(signed) = SChatCommandSigned::read(&mut signed_payload, &version) {
                        signed.command.to_string()
                    } else {
                        SChatCommand::read(&mut payload, &version)?
                            .command
                            .to_string()
                    };
                let client_platform = player.client.clone();
                let player_c = player.clone();
                let server_c = server.clone();
                server.spawn_task(async move {
                    if let ClientPlatform::Java(client) = client_platform.as_ref() {
                        let packet = SChatCommand { command: &cmd };
                        client
                            .handle_chat_command(&player_c, &server_c, &packet)
                            .await;
                    }
                });
            }
            id if id == SChatMessage::to_id(version) => {
                let packet = SChatMessage::read(&mut payload, &version)?;
                let msg = packet.message.to_string();
                let signature = packet.signature.map(<[u8]>::to_vec);
                let ack = packet.acknowledged.to_vec();
                let ts = packet.timestamp;
                let salt = packet.salt;
                let count = packet.message_count;
                let checksum = packet.checksum;
                let client_platform = player.client.clone();
                let player_c = player.clone();
                let server_c = server.clone();
                server.spawn_task(async move {
                    if let ClientPlatform::Java(client) = client_platform.as_ref() {
                        let packet = SChatMessage {
                            message: &msg,
                            timestamp: ts,
                            salt,
                            signature: signature.as_deref(),
                            message_count: count,
                            acknowledged: &ack,
                            checksum,
                        };
                        client
                            .handle_chat_message(&server_c, &player_c, packet)
                            .await;
                    }
                });
            }
            id if id == SClientInformationPlay::to_id(version) => {
                self.handle_client_information(
                    server,
                    player,
                    &SClientInformationPlay::read(&mut payload, &version)?,
                );
            }
            id if id == SClientCommand::to_id(version) => {
                self.handle_client_status(player, &SClientCommand::read(&mut payload, &version)?);
            }
            id if id == SPlayerInput::to_id(version) => {
                self.handle_player_input(
                    player,
                    &SPlayerInput::read(&mut payload, &version)?,
                    server,
                );
            }
            id if id == SMoveVehicle::to_id(version) => {
                self.handle_move_vehicle(player, &SMoveVehicle::read(&mut payload, &version)?);
            }
            id if id == SPaddleBoat::to_id(version) => {
                self.handle_paddle_boat(player, &SPaddleBoat::read(&mut payload, &version)?);
            }
            id if id == SInteract::to_id(version) => {
                self.handle_interact(player, &SInteract::read(&mut payload, &version)?, server);
            }
            id if id == SBundleItemSelected::to_id(version) => {
                self.handle_bundle_item_selected(
                    player,
                    &SBundleItemSelected::read(&mut payload, &version)?,
                );
            }
            id if id == SAttack::to_id(version) => {
                self.handle_attack(player, &SAttack::read(&mut payload, &version)?, server);
            }
            id if id == STeleportToEntity::to_id(version) => {
                self.handle_teleport_to_entity(
                    player,
                    &STeleportToEntity::read(&mut payload, &version)?,
                    server,
                );
            }
            id if id == pumpkin_protocol::java::server::play::SKeepAlive::to_id(version) => {
                self.handle_keep_alive(
                    player,
                    &pumpkin_protocol::java::server::play::SKeepAlive::read(
                        &mut payload,
                        &version,
                    )?,
                );
            }
            id if id == SClientTickEnd::to_id(version) => {
                self.handle_client_tick_end(player);
            }
            id if id == STestInstanceBlockAction::to_id(version) => {
                self.handle_test_instance_block_action(
                    player,
                    &STestInstanceBlockAction::read(&mut payload, &version)?,
                );
            }
            id if id == SSetTestBlock::to_id(version) => {
                self.handle_set_test_block(player, &SSetTestBlock::read(&mut payload, &version)?);
            }
            id if id == SDebugSubscriptionRequest::to_id(version) => {
                self.handle_debug_subscription_request(
                    player,
                    &SDebugSubscriptionRequest::read(&mut payload, &version)?,
                );
            }
            id if id == SDebugSampleSubscription::to_id(version) => {
                self.handle_debug_sample_subscription(
                    player,
                    &SDebugSampleSubscription::read(&mut payload, &version)?,
                );
            }
            id if id == SPlayerPosition::to_id(version) => {
                self.handle_position(
                    player,
                    server,
                    &SPlayerPosition::read(&mut payload, &version)?,
                );
            }
            id if id == SPlayerPositionRotation::to_id(version) => {
                self.handle_position_rotation(
                    player,
                    server,
                    &SPlayerPositionRotation::read(&mut payload, &version)?,
                );
            }
            id if id == SPlayerRotation::to_id(version) => {
                self.handle_rotation(player, &SPlayerRotation::read(&mut payload, &version)?);
            }
            id if id == SSetPlayerGround::to_id(version) => {
                self.handle_player_ground(player, &SSetPlayerGround::read(&mut payload, &version)?);
            }
            id if id == SPickItemFromBlock::to_id(version) => {
                self.handle_pick_item_from_block(
                    player,
                    &SPickItemFromBlock::read(&mut payload, &version)?,
                );
            }
            id if id
                == pumpkin_protocol::java::server::play::SPickItemFromEntity::to_id(version) =>
            {
                self.handle_pick_item_from_entity(
                    player,
                    &pumpkin_protocol::java::server::play::SPickItemFromEntity::read(
                        &mut payload,
                        &version,
                    )?,
                );
            }
            id if id == SPlayerAbilities::to_id(version) => {
                self.handle_player_abilities(
                    player,
                    &SPlayerAbilities::read(&mut payload, &version)?,
                    server,
                );
            }
            id if id == SPlayerAction::to_id(version) => {
                self.handle_player_action(
                    player,
                    &SPlayerAction::read(&mut payload, &version)?,
                    server,
                );
            }
            id if id == SSetCommandBlock::to_id(version) => {
                self.handle_set_command_block(
                    player,
                    &SSetCommandBlock::read(&mut payload, &version)?,
                );
            }
            id if id == SSetJigsawBlock::to_id(version) => {
                self.handle_set_jigsaw_block(
                    player,
                    &SSetJigsawBlock::read(&mut payload, &version)?,
                );
            }
            id if id == SJigsawGenerate::to_id(version) => {
                self.handle_jigsaw_generate(
                    player,
                    &SJigsawGenerate::read(&mut payload, &version)?,
                );
            }
            id if id == SPlayerCommand::to_id(version) => {
                self.handle_player_command(
                    player,
                    &SPlayerCommand::read(&mut payload, &version)?,
                    server,
                );
            }
            id if id == SPlayerLoaded::to_id(version) => {
                Self::handle_player_loaded(player);
            }
            id if id == SPlayPingRequest::to_id(version) => {
                self.handle_play_ping_request(&SPlayPingRequest::read(&mut payload, &version)?);
            }
            id if id == SClickSlot::to_id(version) => {
                player.on_slot_click(SClickSlot::read(&mut payload, &version)?, server);
            }
            id if id == SContainerButtonClick::to_id(version) => {
                player.on_container_button_click(&SContainerButtonClick::read(
                    &mut payload,
                    &version,
                )?);
            }
            id if id == SSetHeldItem::to_id(version) => {
                self.handle_set_held_item(
                    server,
                    player,
                    &SSetHeldItem::read(&mut payload, &version)?,
                );
            }
            id if id == SSetCreativeSlot::to_id(version) => {
                self.handle_set_creative_slot(
                    player,
                    SSetCreativeSlot::read(&mut payload, &version)?,
                )?;
            }
            id if id == SSwingArm::to_id(version) => {
                self.handle_swing_arm(server, player, &SSwingArm::read(&mut payload, &version)?);
            }
            id if id == SUpdateSign::to_id(version) => {
                self.handle_sign_update(player, &SUpdateSign::read(&mut payload, &version)?);
            }
            id if id == SEditBook::to_id(version) => {
                self.handle_edit_book(player, &SEditBook::read(&mut payload, &version)?);
            }
            id if id == SUseItemOn::to_id(version) => {
                self.handle_use_item_on(
                    player,
                    &SUseItemOn::read(&mut payload, &version)?,
                    server,
                )?;
            }
            id if id == SUseItem::to_id(version) => {
                self.handle_use_item(player, &SUseItem::read(&mut payload, &version)?, server);
            }
            id if id == SCommandSuggestion::to_id(version) => {
                self.handle_command_suggestion(
                    player,
                    &SCommandSuggestion::read(&mut payload, &version)?,
                    server,
                );
            }
            id if id == SPCookieResponse::to_id(version) => {
                self.handle_cookie_response(&SPCookieResponse::read(&mut payload, &version)?);
            }
            id if id == SCloseContainer::to_id(version) => {
                let packet = SCloseContainer::read(&mut payload, &version)?;
                self.handle_close_container(player, packet.window_id.0);
            }
            id if id == SChunkBatch::to_id(version) => {
                self.handle_chunk_batch(player, &SChunkBatch::read(&mut payload, &version)?);
            }
            id if id == SPlayerSession::to_id(version) => {
                let session = SPlayerSession::read(&mut payload, &version)?;
                let client_platform = player.client.clone();
                let player_c = player.clone();
                let server_c = server.clone();
                server.spawn_task(async move {
                    if let ClientPlatform::Java(client) = client_platform.as_ref() {
                        client
                            .handle_chat_session_update(&player_c, &server_c, session)
                            .await;
                    }
                });
            }
            id if id == SCustomPayload::to_id(version) => {
                let payload = SCustomPayload::read(&mut payload, &version)?;
                let channel_str = payload.channel.to_string();
                let mut event = PlayerCustomPayloadEvent::new(
                    player.clone(),
                    channel_str.clone(),
                    Bytes::copy_from_slice(payload.data),
                );
                server.plugin_manager.fire_blocking(server, &mut event);

                if channel_str == "minecraft:register" {
                    if let Ok(channels_data) = std::str::from_utf8(payload.data) {
                        for ch in channels_data.split('\0') {
                            if !ch.is_empty() {
                                let mut reg_event = crate::plugin::api::events::player::player_register_channel::PlayerRegisterChannelEvent::new(
                                    player.clone(),
                                    ch.to_string(),
                                );
                                server.plugin_manager.fire_blocking(server, &mut reg_event);
                                let mut ch_event = crate::plugin::api::events::player::player_channel::PlayerChannelEvent {
                                    player: player.clone(),
                                    channel: ch.to_string(),
                                    cancelled: false,
                                };
                                server.plugin_manager.fire_blocking(server, &mut ch_event);
                            }
                        }
                    }
                } else if channel_str == "minecraft:unregister"
                    && let Ok(channels_data) = std::str::from_utf8(payload.data)
                {
                    for ch in channels_data.split('\0') {
                        if !ch.is_empty() {
                            let mut unreg_event = crate::plugin::api::events::player::player_unregister_channel::PlayerUnregisterChannelEvent::new(
                                player.clone(),
                                ch.to_string(),
                            );
                            server
                                .plugin_manager
                                .fire_blocking(server, &mut unreg_event);
                        }
                    }
                }
            }
            id if id == SRecipeBookChangeSettings::to_id(version) => {
                self.handle_recipe_book_change_settings(
                    server,
                    player,
                    &SRecipeBookChangeSettings::read(&mut payload, &version)?,
                );
            }
            id if id == SRecipeBookSeenRecipe::to_id(version) => {
                self.handle_recipe_book_seen_recipe(
                    server,
                    player,
                    &SRecipeBookSeenRecipe::read(&mut payload, &version)?,
                );
            }
            id if id == SRenameItem::to_id(version) => {
                player.on_rename_item(&SRenameItem::read(&mut payload, &version)?);
            }
            id if id == SPlaceRecipe::to_id(version) => {
                let packet = SPlaceRecipe::read(&mut payload, &version)?;
                self.handle_place_recipe(server, player, &packet);
            }
            id if id
                == pumpkin_protocol::java::server::play::SCustomClickAction::to_id(version) =>
            {
                let packet = pumpkin_protocol::java::server::play::SCustomClickAction::read(
                    &mut payload,
                    &version,
                )?;
                let mut event = crate::plugin::api::events::dialog::dialog_click_action::DialogClickActionEvent::new(
                    player.clone(),
                    packet.action_id.to_string(),
                    packet.payload.map(Bytes::copy_from_slice),
                );
                server.plugin_manager.fire_blocking(server, &mut event);
            }
            id if id == SSelectTrade::to_id(version) => {
                self.handle_select_trade(player, &SSelectTrade::read(&mut payload, &version)?);
            }
            id if id == SSeenAdvancement::to_id(version) => {
                self.handle_seen_advancement(
                    player,
                    &SSeenAdvancement::read(&mut payload, &version)?,
                );
            }
            id if id == SPlayResourcePack::to_id(version) => {
                self.handle_play_resource_pack_response(
                    server,
                    player,
                    &SPlayResourcePack::read(&mut payload, &version)?,
                );
            }
            id if id == SPlayPong::to_id(version) => {
                self.handle_play_pong(player, &SPlayPong::read(&mut payload, &version)?);
            }
            id if id == SLockDifficulty::to_id(version) => {
                self.handle_lock_difficulty(
                    server,
                    player,
                    &SLockDifficulty::read(&mut payload, &version)?,
                );
            }
            id if id == SChangeDifficulty::to_id(version) => {
                self.handle_change_difficulty(
                    server,
                    player,
                    &SChangeDifficulty::read(&mut payload, &version)?,
                );
            }
            id if id == SSetBeacon::to_id(version) => {
                self.handle_set_beacon(player, &SSetBeacon::read(&mut payload, &version)?);
            }
            id if id == SContainerSlotStateChanged::to_id(version) => {
                self.handle_container_slot_state_changed(
                    player,
                    &SContainerSlotStateChanged::read(&mut payload, &version)?,
                );
            }
            id if id == SSpectateEntity::to_id(version) => {
                self.handle_spectate_entity(
                    player,
                    server,
                    &SSpectateEntity::read(&mut payload, &version)?,
                );
            }
            id if id == SSetCommandMinecart::to_id(version) => {
                self.handle_set_command_minecart(
                    player,
                    &SSetCommandMinecart::read(&mut payload, &version)?,
                );
            }
            id if id == SSetStructureBlock::to_id(version) => {
                self.handle_set_structure_block(
                    player,
                    &SSetStructureBlock::read(&mut payload, &version)?,
                );
            }
            id if id == SSetGameRule::to_id(version) => {
                self.handle_set_game_rule(player, &SSetGameRule::read(&mut payload, &version)?);
            }
            id if id == SBlockEntityTagQuery::to_id(version) => {
                self.handle_block_entity_tag_query(
                    player,
                    &SBlockEntityTagQuery::read(&mut payload, &version)?,
                );
            }
            id if id == SEntityTagQuery::to_id(version) => {
                self.handle_entity_tag_query(
                    player,
                    &SEntityTagQuery::read(&mut payload, &version)?,
                );
            }
            id if id == SConfigurationAcknowledged::to_id(version) => {
                let _ = SConfigurationAcknowledged::read(&mut payload, &version)?;
                if !payload.is_empty() {
                    return Err(Box::new(ReadingError::Message(
                        "Trailing data in configuration acknowledgment".into(),
                    )));
                }
                self.handle_configuration_acknowledged(player);
            }
            _ => {
                warn!("Failed to handle player packet id {}", event.packet_id);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod configuration_state_tests {
    use super::{
        ConfigurationPhase, KnownPacksSelection, ResourcePackResponseAction, claim_finish,
        claim_reconfiguration, claim_reconfiguration_ack, registry_entries_for_known_packs,
        require_empty_body, resource_pack_response_action, resource_pack_uuid,
        resource_pack_uuid_matches,
    };
    use crossbeam::atomic::AtomicCell;
    use pumpkin_protocol::{KnownPack, java::server::config::ResourcePackResponseResult};
    use std::sync::{Arc, Mutex};

    #[test]
    fn known_packs_exact_list_preserves_order_and_rejects_subset_unknown_and_duplicates() {
        let state = Arc::new(Mutex::new(None));
        super::remember_known_packs(
            &state,
            &[
                KnownPack {
                    namespace: "minecraft",
                    id: "core",
                    version: "26.2",
                },
                KnownPack {
                    namespace: "minecraft",
                    id: "bundle",
                    version: "26.2",
                },
            ],
        );

        let exact = [
            KnownPack {
                namespace: "minecraft",
                id: "core",
                version: "26.2",
            },
            KnownPack {
                namespace: "minecraft",
                id: "bundle",
                version: "26.2",
            },
        ];
        assert_eq!(
            super::select_known_packs(&state, &exact),
            KnownPacksSelection::Exact
        );

        let subset = [KnownPack {
            namespace: "minecraft",
            id: "core",
            version: "26.2",
        }];
        assert_eq!(
            super::select_known_packs(&state, &subset),
            KnownPacksSelection::Fallback
        );
        let reordered = [
            KnownPack {
                namespace: "minecraft",
                id: "bundle",
                version: "26.2",
            },
            KnownPack {
                namespace: "minecraft",
                id: "core",
                version: "26.2",
            },
        ];
        assert_eq!(
            super::select_known_packs(&state, &reordered),
            KnownPacksSelection::Fallback
        );
        let duplicate = [
            KnownPack {
                namespace: "minecraft",
                id: "core",
                version: "26.2",
            },
            KnownPack {
                namespace: "minecraft",
                id: "core",
                version: "26.2",
            },
        ];
        assert_eq!(
            super::select_known_packs(&state, &duplicate),
            KnownPacksSelection::Fallback
        );
        let unknown = [
            KnownPack {
                namespace: "minecraft",
                id: "core",
                version: "26.2",
            },
            KnownPack {
                namespace: "example",
                id: "unknown",
                version: "1",
            },
        ];
        assert_eq!(
            super::select_known_packs(&state, &unknown),
            KnownPacksSelection::Fallback
        );
        assert_eq!(
            super::select_known_packs(&state, &[]),
            KnownPacksSelection::Fallback
        );
    }

    #[test]
    fn resource_pack_status_matrix_waits_for_download_success_and_applies_force_policy() {
        assert_eq!(
            resource_pack_response_action(&ResourcePackResponseResult::Accepted, true),
            ResourcePackResponseAction::Wait
        );
        assert_eq!(
            resource_pack_response_action(&ResourcePackResponseResult::Downloaded, true),
            ResourcePackResponseAction::Wait
        );
        assert_eq!(
            resource_pack_response_action(&ResourcePackResponseResult::DownloadSuccess, true),
            ResourcePackResponseAction::Complete
        );
        assert_eq!(
            resource_pack_response_action(&ResourcePackResponseResult::Declined, false),
            ResourcePackResponseAction::Complete
        );
        assert_eq!(
            resource_pack_response_action(&ResourcePackResponseResult::DownloadFail, true),
            ResourcePackResponseAction::Complete
        );
        assert_eq!(
            resource_pack_response_action(&ResourcePackResponseResult::DownloadFail, false),
            ResourcePackResponseAction::Complete
        );
        assert_eq!(
            resource_pack_response_action(&ResourcePackResponseResult::Discarded, true),
            ResourcePackResponseAction::Complete
        );
        assert_eq!(
            resource_pack_response_action(&ResourcePackResponseResult::InvalidUrl, true),
            ResourcePackResponseAction::Complete
        );
        assert_eq!(
            resource_pack_response_action(&ResourcePackResponseResult::ReloadFailed, false),
            ResourcePackResponseAction::Complete
        );
    }

    #[test]
    fn resource_pack_uuid_mismatch_is_not_the_configured_pack() {
        assert!(!resource_pack_uuid_matches(
            "https://example.invalid/a.zip",
            resource_pack_uuid("https://example.invalid/b.zip"),
        ));
    }

    #[test]
    fn known_packs_registry_fallback_does_not_skip_configuration_data() {
        let registry = pumpkin_data::registry::Registry {
            registry_id: "minecraft:test".to_owned(),
            registry_entries: vec![pumpkin_data::registry::RegistryEntryData {
                entry_id: "minecraft:entry".to_owned(),
                data: Some(vec![1, 2, 3].into_boxed_slice()),
            }],
        };
        assert_eq!(
            registry_entries_for_known_packs(&registry, KnownPacksSelection::Exact).len(),
            1
        );
        assert_eq!(
            registry_entries_for_known_packs(&registry, KnownPacksSelection::Fallback).len(),
            1
        );
        assert!(!ConfigurationPhase::AwaitingKnownPacks.accepts_finish_ack());
    }

    #[test]
    fn reconfiguration_dispatch_transitions_twice_without_duplicate_ack_or_finish() {
        let phase = AtomicCell::new(ConfigurationPhase::Play);

        assert!(require_empty_body(&[], "test packet").is_ok());
        assert!(require_empty_body(&[0], "test packet").is_err());

        // An unsolicited Play acknowledgment is rejected without publishing a
        // new phase. The legal empty-body acknowledgment is accepted once.
        assert!(!claim_reconfiguration_ack(&phase));
        assert_eq!(phase.load(), ConfigurationPhase::Play);
        assert!(claim_reconfiguration(&phase));
        assert!(!claim_reconfiguration(&phase));
        assert_eq!(
            phase.load(),
            ConfigurationPhase::ReconfigurationAwaitingStart
        );
        phase.store(ConfigurationPhase::ReconfigurationAwaitingAck);
        assert!(claim_reconfiguration_ack(&phase));
        assert_eq!(phase.load(), ConfigurationPhase::AwaitingKnownPacks);
        assert!(!claim_reconfiguration_ack(&phase));
        assert_eq!(phase.load(), ConfigurationPhase::AwaitingKnownPacks);
        assert!(!claim_finish(&phase));

        // Finish acknowledgment is also exactly once, and a second complete
        // transition retains the same state machine rather than rebuilding a player.
        phase.store(ConfigurationPhase::AwaitingFinishAck);
        assert!(claim_finish(&phase));
        assert_eq!(phase.load(), ConfigurationPhase::Play);
        assert!(!claim_finish(&phase));
        assert_eq!(phase.load(), ConfigurationPhase::Play);

        assert!(claim_reconfiguration(&phase));
        phase.store(ConfigurationPhase::ReconfigurationAwaitingAck);
        assert!(claim_reconfiguration_ack(&phase));
        phase.store(ConfigurationPhase::AwaitingFinishAck);
        assert!(claim_finish(&phase));
        assert_eq!(phase.load(), ConfigurationPhase::Play);
        assert!(!claim_finish(&phase));
    }
}
