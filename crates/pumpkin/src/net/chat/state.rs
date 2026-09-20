//! Transactional state for inbound 26.2 secure-chat messages.
//!
//! `ChatSession` currently does not own the inbound chain cursor, so this module
//! keeps the small inbound cursor in a UUID-keyed sidecar. Ownership is supplied
//! by a per-Player capability rather than a historical UUID tombstone. Ack state
//! is never mutated until canonical signature verification succeeds.

use std::collections::{HashMap, hash_map::Entry};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use pumpkin_protocol::java::server::play::SChatMessage;
use thiserror::Error;
use uuid::Uuid;

use crate::entity::player::{LastSeenMessagesValidator, Player};

/// Per-`Player` lifecycle capability used instead of a process-lifetime tombstone.
///
/// A disconnected or superseded player can retain an async task, but its token is
/// permanently inactive. A replacement receives a different token, so rejecting
/// late work does not require retaining one tombstone for every historical UUID.
pub(crate) struct ChatOwnerToken {
    active: AtomicBool,
}

impl ChatOwnerToken {
    #[must_use]
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            active: AtomicBool::new(true),
        })
    }

    pub(crate) fn retire(&self) {
        self.active.store(false, Ordering::Release);
    }

    #[must_use]
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

use super::signature::{ChatSignatureError, verify_chat_message_signature};
use super::{SignedMessageBody, SignedMessageLink};

const ACK_UPDATE_LEN: usize = 3;

#[derive(Clone, Debug)]
struct InboundChatState {
    session_id: Uuid,
    /// Weak identity of the live Player owner. The Player token is retired on
    /// disconnect or duplicate-login supersession, so this state does not keep
    /// historical owners alive.
    owner: Weak<ChatOwnerToken>,
    next_index: i32,
    last_timestamp_epoch_second: Option<i64>,
    last_timestamp_epoch_millis: Option<i64>,
    broken: bool,
}

impl InboundChatState {
    fn new(session_id: Uuid) -> Self {
        Self {
            session_id,
            owner: Weak::new(),
            next_index: 0,
            last_timestamp_epoch_second: None,
            last_timestamp_epoch_millis: None,
            broken: false,
        }
    }

    fn check_link_at(
        &self,
        session_id: Uuid,
        index: i32,
        timestamp_epoch_millis: i64,
    ) -> Result<(), ChatStateError> {
        if self.broken {
            return Err(ChatStateError::ChainBroken);
        }
        if self.session_id != session_id {
            return Err(ChatStateError::SessionMismatch);
        }
        if index != self.next_index {
            return Err(ChatStateError::Replay {
                expected: self.next_index,
                received: index,
            });
        }
        if self
            .last_timestamp_epoch_millis
            .is_some_and(|last| timestamp_epoch_millis < last)
        {
            return Err(ChatStateError::OutOfOrderTimestamp);
        }
        Ok(())
    }

    fn advance_at(
        &mut self,
        timestamp_epoch_millis: i64,
        timestamp_epoch_second: i64,
    ) -> Result<(), ChatStateError> {
        self.next_index = self
            .next_index
            .checked_add(1)
            .ok_or(ChatStateError::IndexOverflow)?;
        self.last_timestamp_epoch_second = Some(timestamp_epoch_second);
        self.last_timestamp_epoch_millis = Some(timestamp_epoch_millis);
        Ok(())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ChatStateError {
    #[error("chat session does not match inbound chain state")]
    SessionMismatch,
    #[error("chat message index is not the next inbound index")]
    Replay { expected: i32, received: i32 },
    #[error("chat message timestamp is older than the last accepted timestamp")]
    OutOfOrderTimestamp,
    #[error("secure chat message chain is broken")]
    ChainBroken,
    #[error("chat message index overflowed")]
    IndexOverflow,
    #[error("chat connection lifecycle no longer owns this state")]
    LifecycleMismatch,
    #[error("last-seen acknowledgement update is invalid")]
    AckValidation,
    #[error("too many unacknowledged chats queued")]
    TooManyPendingChats,
    #[error("last-seen checksum mismatch")]
    ChecksumMismatch { expected: u8, received: u8 },
    #[error("chat signature verification failed: {0}")]
    Signature(#[from] ChatSignatureError),
}

#[derive(Default)]
struct InboundStateStore {
    /// Only currently installed UUID state is retained. Retired ownership lives
    /// in the Player token and therefore disappears with that Player/task graph.
    states: HashMap<Uuid, InboundChatState>,
}

static INBOUND_STATE: OnceLock<Mutex<InboundStateStore>> = OnceLock::new();

fn inbound_state() -> &'static Mutex<InboundStateStore> {
    INBOUND_STATE.get_or_init(|| Mutex::new(InboundStateStore::default()))
}

/// Installs the inbound cursor after the existing session certificate checks pass.
///
/// Repeating the exact same session update is deliberately a no-op. The
/// Player token is the lifecycle owner; a delayed old connection cannot
/// replace a newer owner, and a retired owner cannot recreate a cleared state.
pub(crate) fn reset_inbound_state(
    player_id: Uuid,
    session_id: Uuid,
    owner: &Arc<ChatOwnerToken>,
) -> bool {
    if !owner.is_active() {
        return false;
    }
    let mut store = inbound_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !owner.is_active() {
        return false;
    }
    if let Some(current_owner) = store
        .states
        .get(&player_id)
        .and_then(|state| state.owner.upgrade())
        && !Arc::ptr_eq(&current_owner, owner)
        && current_owner.is_active()
    {
        // Server::add_player retires the previous duplicate-UUID owner before
        // the replacement installs chat state. Refuse an active split-brain
        // owner as a defensive invariant if that publication is bypassed.
        return false;
    }
    match store.states.entry(player_id) {
        Entry::Occupied(mut entry) => {
            let state = entry.get_mut();
            if state
                .owner
                .upgrade()
                .is_some_and(|current| Arc::ptr_eq(&current, owner))
                && state.session_id == session_id
            {
                return true;
            }
            *state = InboundChatState {
                session_id,
                owner: Arc::downgrade(owner),
                ..InboundChatState::new(session_id)
            };
        }
        Entry::Vacant(entry) => {
            entry.insert(InboundChatState {
                session_id,
                owner: Arc::downgrade(owner),
                ..InboundChatState::new(session_id)
            });
        }
    }
    true
}

/// Marks the current player's inbound signed chain broken after a signed
/// command name-set mismatch. The lifecycle owner prevents stale players from
/// breaking a replacement session.
pub fn break_inbound_chain(player: &Player) -> bool {
    let player_id = player.gameprofile.id;
    let mut store = inbound_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(state) = store.states.get_mut(&player_id) else {
        return false;
    };
    if !player.chat_owner.is_active()
        || !state
            .owner
            .upgrade()
            .is_some_and(|owner| Arc::ptr_eq(&owner, &player.chat_owner))
    {
        return false;
    }
    state.broken = true;
    true
}

/// Clears only a matching installed session for the supplied owner token.
///
/// The caller retires the token while holding the Player lifecycle lock before
/// calling this function. That is the ABA boundary: after retirement, a late
/// task cannot recreate or mutate the UUID-keyed sidecar.
pub(crate) fn clear_inbound_state(
    player_id: Uuid,
    session_id: Uuid,
    owner: &Arc<ChatOwnerToken>,
) -> bool {
    let mut store = inbound_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if store.states.get(&player_id).is_some_and(|state| {
        state.session_id == session_id
            && state
                .owner
                .upgrade()
                .is_some_and(|current| Arc::ptr_eq(&current, owner))
    }) {
        store.states.remove(&player_id);
        true
    } else {
        false
    }
}

/// Verifies and commits one secure chat message as one transaction.
///
/// The sidecar lock is held across the ack preview and RSA verification so two
/// concurrent packets cannot both accept the same inbound index. The player's
/// original ack validator and this sidecar are written only after every check
/// succeeds.
pub fn verify_and_commit(
    player: &Player,
    session_id: Uuid,
    public_key: &[u8],
    chat_message: &SChatMessage<'_>,
) -> Result<(), ChatStateError> {
    let signature = chat_message
        .signature
        .ok_or(ChatSignatureError::InvalidSignatureLength {
            length: 0,
            expected: super::signature::CHAT_SIGNATURE_LEN,
        })?;
    let contents = [chat_message.message];
    let signatures = [signature];
    verify_and_commit_entries(
        player,
        session_id,
        public_key,
        chat_message.timestamp,
        chat_message.salt,
        chat_message.message_count.0,
        chat_message.acknowledged,
        (chat_message.checksum != 0).then_some(chat_message.checksum),
        &contents,
        &signatures,
    )
}

/// Applies the packet's last-seen update before command parsing/authentication.
///
/// Vanilla calls `unpackAndApplyLastSeen` first for signed commands.  Keep this
/// mutation separate from argument verification so a later command rejection
/// does not roll back an already accepted ACK update.
pub fn apply_last_seen_update(
    player: &Player,
    message_count: i32,
    acknowledged: &[u8],
    checksum: u8,
) -> Result<Vec<Box<[u8]>>, ChatStateError> {
    let mut cache = player
        .signature_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (validator, last_seen) =
        preview_last_seen(&cache.last_seen_validator, message_count, acknowledged)?;
    if validator.tracked_messages_count() > 4096 {
        return Err(ChatStateError::TooManyPendingChats);
    }
    if checksum != 0 {
        let expected = last_seen_checksum(&last_seen);
        if expected != checksum {
            return Err(ChatStateError::ChecksumMismatch {
                expected,
                received: checksum,
            });
        }
    }
    cache.last_seen_validator = validator;
    Ok(last_seen)
}

/// Verifies command argument signatures in packet-entry order.
///
/// Each successful entry advances the live chain immediately, just as the
/// vanilla decoder advances after each `unpack`.  A later failure therefore
/// preserves earlier advances and leaves the chain broken.
pub fn verify_signed_command_entries(
    player: &Player,
    session_id: Uuid,
    public_key: &[u8],
    timestamp: i64,
    salt: i64,
    last_seen: &[Box<[u8]>],
    contents: &[&str],
    signatures: &[&[u8]],
) -> Result<(), ChatStateError> {
    if contents.len() != signatures.len() {
        return Err(ChatStateError::AckValidation);
    }

    let player_id = player.gameprofile.id;
    let timestamp_epoch_second = timestamp.div_euclid(1_000);
    let mut store = inbound_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(state) = store.states.get_mut(&player_id) else {
        return Err(ChatStateError::LifecycleMismatch);
    };
    if !player.chat_owner.is_active()
        || state.session_id != session_id
        || !state
            .owner
            .upgrade()
            .is_some_and(|owner| Arc::ptr_eq(&owner, &player.chat_owner))
    {
        return Err(ChatStateError::LifecycleMismatch);
    }

    for (content, signature) in contents.iter().zip(signatures) {
        let index = state.next_index;
        if let Err(error) = state.check_link_at(session_id, index, timestamp) {
            if matches!(error, ChatStateError::OutOfOrderTimestamp) {
                state.broken = true;
            }
            return Err(error);
        }

        let link = SignedMessageLink::new(index, player_id, session_id);
        let body =
            SignedMessageBody::new((*content).to_string(), timestamp, salt, last_seen.to_vec());
        // Vanilla records the timestamp before RSA verification.  The chain is
        // broken on failure, so this is observable only through diagnostics.
        state.last_timestamp_epoch_second = Some(timestamp_epoch_second);
        state.last_timestamp_epoch_millis = Some(timestamp);
        if let Err(error) = verify_chat_message_signature(public_key, &link, &body, signature) {
            state.broken = true;
            return Err(error.into());
        }
        state.advance_at(timestamp, timestamp_epoch_second)?;
    }
    Ok(())
}

/// Verifies all signed command arguments with the existing ACK/state helpers.
/// ACK state is committed first; argument links are committed one by one.
pub fn verify_signed_command_and_commit(
    player: &Player,
    session_id: Uuid,
    public_key: &[u8],
    timestamp: i64,
    salt: i64,
    message_count: i32,
    acknowledged: &[u8],
    checksum: u8,
    contents: &[&str],
    signatures: &[&[u8]],
) -> Result<(), ChatStateError> {
    let last_seen = apply_last_seen_update(player, message_count, acknowledged, checksum)?;
    verify_signed_command_entries(
        player, session_id, public_key, timestamp, salt, &last_seen, contents, signatures,
    )
}

fn verify_and_commit_entries(
    player: &Player,
    session_id: Uuid,
    public_key: &[u8],
    timestamp: i64,
    salt: i64,
    message_count: i32,
    acknowledged: &[u8],
    checksum: Option<u8>,
    contents: &[&str],
    signatures: &[&[u8]],
) -> Result<(), ChatStateError> {
    if contents.len() != signatures.len() {
        return Err(ChatStateError::AckValidation);
    }
    let last_seen = apply_last_seen_update(
        player,
        message_count,
        acknowledged,
        checksum.unwrap_or_default(),
    )?;
    verify_signed_command_entries(
        player, session_id, public_key, timestamp, salt, &last_seen, contents, signatures,
    )
}

/// Applies one packet's ack update to a clone of the tracked window.
///
/// The original validator is never changed, including when the update fails.
pub fn preview_last_seen(
    original: &LastSeenMessagesValidator,
    offset: i32,
    acknowledged: &[u8],
) -> Result<(LastSeenMessagesValidator, Vec<Box<[u8]>>), ChatStateError> {
    if offset < 0 {
        return Err(ChatStateError::AckValidation);
    }
    let mut preview = original.clone();
    let last_seen = if acknowledged.is_empty() {
        preview
            .apply_offset(offset as usize)
            .map_err(|_| ChatStateError::AckValidation)?;
        Vec::new()
    } else {
        if acknowledged.len() != ACK_UPDATE_LEN {
            return Err(ChatStateError::AckValidation);
        }
        preview
            .apply_update(offset as usize, acknowledged)
            .map_err(|_| ChatStateError::AckValidation)?
    };
    Ok((preview, last_seen))
}

/// The 26.2 `LastSeenMessages.computeChecksum` byte result.
#[must_use]
pub fn last_seen_checksum(signatures: &[Box<[u8]>]) -> u8 {
    let mut hash = 1_i32;
    for signature in signatures {
        hash = hash
            .wrapping_mul(31)
            .wrapping_add(java_byte_array_hash(signature));
    }
    let checksum = hash as u8;
    if checksum == 0 { 1 } else { checksum }
}

fn java_byte_array_hash(bytes: &[u8]) -> i32 {
    bytes.iter().fold(1_i32, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(i32::from(*byte as i8))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ChatOwnerToken, ChatStateError, InboundChatState, clear_inbound_state, inbound_state,
        last_seen_checksum, preview_last_seen, reset_inbound_state, verify_and_commit,
        verify_signed_command_and_commit,
    };
    use crate::entity::player::{ChatSession, LastSeenMessagesValidator, LastSeenTrackedEntry};
    use crate::net::chat::signature::canonical_bytes;
    use crate::net::chat::{SignedMessageBody, SignedMessageLink};
    use crate::net::java::JavaClient;
    use crate::net::java::pending::PendingConnection;
    use crate::net::{ClientPlatform, GameProfile, PacketRateLimiter, PlayerConfig};
    use arc_swap::ArcSwap;
    use pumpkin_config::{AdvancedConfiguration, BasicConfiguration, TelemetryConfig};
    use pumpkin_data::dimension::Dimension;
    use pumpkin_protocol::codec::var_int::VarInt;
    use pumpkin_protocol::java::server::play::SChatMessage;
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::net::{TcpListener, TcpStream};
    use uuid::Uuid;

    fn state() -> InboundChatState {
        InboundChatState::new(Uuid::from_u128(10))
    }

    fn owner() -> Arc<ChatOwnerToken> {
        ChatOwnerToken::new()
    }

    fn test_vanilla_data() -> crate::data::VanillaData {
        crate::data::VanillaData {
            banned_ip_list: std::sync::RwLock::new(Default::default()),
            banned_player_list: std::sync::RwLock::new(Default::default()),
            operator_config: std::sync::RwLock::new(Default::default()),
            user_cache: std::sync::RwLock::new(Default::default()),
            whitelist_config: std::sync::RwLock::new(Default::default()),
        }
    }

    fn fixture_bytes(value: &str) -> Vec<u8> {
        hex::decode(value).expect("fixture hex")
    }

    async fn runtime_java_client(profile: &GameProfile) -> Arc<ClientPlatform> {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("fixture listener");
        let address = listener.local_addr().expect("fixture listener address");
        let connector = tokio::spawn(TcpStream::connect(address));
        let (server_stream, peer_address) = listener.accept().await.expect("fixture accept");
        let _peer = connector
            .await
            .expect("fixture connector task")
            .expect("fixture connect");
        let pending = PendingConnection::new(
            server_stream,
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

    #[test]
    fn root_and_next_indices_advance_only_after_success() {
        let mut state = state();
        assert_eq!(state.check_link_at(Uuid::from_u128(10), 0, 1), Ok(()));
        state.advance_at(1, 1).expect("root commit");
        assert_eq!(state.next_index, 1);
        assert_eq!(state.last_timestamp_epoch_second, Some(1));
        assert_eq!(
            state.check_link_at(Uuid::from_u128(10), 0, 2),
            Err(ChatStateError::Replay {
                expected: 1,
                received: 0,
            })
        );
        assert_eq!(state.next_index, 1);
        assert_eq!(state.last_timestamp_epoch_second, Some(1));
    }

    #[test]
    fn n_argument_chain_links_consume_sequential_indices() {
        let mut state = state();
        for index in 0..3 {
            assert_eq!(state.check_link_at(Uuid::from_u128(10), index, 100), Ok(()));
            state.advance_at(100, 100).expect("argument link commit");
        }
        assert_eq!(state.next_index, 3);
        assert_eq!(state.last_timestamp_epoch_second, Some(100));
    }

    #[test]
    fn old_timestamp_is_separate_and_does_not_advance() {
        let mut state = state();
        state.advance_at(1_000, 1_000).expect("root commit");
        assert_eq!(
            state.check_link_at(Uuid::from_u128(10), 1, 999),
            Err(ChatStateError::OutOfOrderTimestamp)
        );
        assert_eq!(state.next_index, 1);
        assert_eq!(state.last_timestamp_epoch_second, Some(1_000));
    }

    #[test]
    fn missing_millisecond_state_does_not_compare_epoch_seconds_as_millis() {
        let mut state = state();
        state.last_timestamp_epoch_second = Some(1_000);
        assert_eq!(state.check_link_at(Uuid::from_u128(10), 0, 999), Ok(()));
    }

    #[test]
    fn timestamp_order_keeps_millisecond_precision() {
        let mut state = state();
        state.advance_at(1_001, 1).expect("timestamp commit");
        assert_eq!(
            state.check_link_at(Uuid::from_u128(10), 1, 1_000),
            Err(ChatStateError::OutOfOrderTimestamp)
        );
    }

    #[test]
    fn chain_break_is_sticky_after_replay_or_signature_failure() {
        let mut state = state();
        state.advance_at(1_000, 1_000).expect("root commit");
        state.broken = true;
        assert_eq!(
            state.check_link_at(Uuid::from_u128(10), 1, 1_001),
            Err(ChatStateError::ChainBroken)
        );
    }

    #[test]
    fn checksum_uses_java_signed_bytes_and_order() {
        let a = vec![0_u8; 256].into_boxed_slice();
        let c = vec![0x80_u8; 256].into_boxed_slice();
        assert_eq!(last_seen_checksum(&[a, c]), 0xe1);
    }

    #[test]
    fn nonempty_last_seen_matches_fixture_hash() {
        let link = SignedMessageLink::new(0, Uuid::from_u128(1), Uuid::from_u128(2));
        let body = SignedMessageBody::new(
            "hi".to_string(),
            1_700_000_000_000,
            0x0102_0304_0506_0708,
            vec![
                vec![0_u8; 256].into_boxed_slice(),
                vec![0x80_u8; 256].into_boxed_slice(),
            ],
        );
        let payload = canonical_bytes(&link, &body).expect("canonical bytes");
        assert_eq!(payload.len(), 578);
        assert_eq!(
            hex::encode(Sha256::digest(payload)),
            "988d6b4bf2a1c50a991d285197ba0eb2c3f7300142262f3f20b35241259e3992"
        );
    }

    #[test]
    fn ack_preview_preserves_order_and_original_on_rejection() {
        let mut original = LastSeenMessagesValidator::new(4);
        let a = vec![0_u8; 256];
        let b = vec![1_u8; 256];
        let c = vec![0x80_u8; 256];
        let d = vec![0xff_u8; 256];
        original.tracked_messages.clear();
        for signature in [&a, &b, &c, &d] {
            original
                .tracked_messages
                .push_back(Some(LastSeenTrackedEntry {
                    signature: signature.clone().into_boxed_slice(),
                    pending: true,
                }));
        }
        let snapshot = original.clone();
        let (preview, acknowledged) =
            preview_last_seen(&original, 0, &[0x05, 0, 0]).expect("ack preview");
        assert_eq!(
            acknowledged,
            vec![a.clone().into_boxed_slice(), c.into_boxed_slice()]
        );
        assert_eq!(
            preview.tracked_messages_count(),
            original.tracked_messages_count()
        );
        assert_eq!(original.tracked_messages, snapshot.tracked_messages);
        let mut offset_fixture = original.clone();
        offset_fixture.tracked_messages.push_back(None);
        offset_fixture.tracked_messages.push_back(None);
        let (_, offset_acknowledged) =
            preview_last_seen(&offset_fixture, 2, &[0x01, 0, 0]).expect("offset ack preview");
        assert_eq!(
            offset_acknowledged,
            vec![vec![0x80_u8; 256].into_boxed_slice()]
        );

        let mut unknown_fixture = LastSeenMessagesValidator::new(4);
        unknown_fixture.tracked_messages[0] = Some(LastSeenTrackedEntry {
            signature: a.into_boxed_slice(),
            pending: true,
        });
        let unknown_snapshot = unknown_fixture.clone();
        assert!(matches!(
            preview_last_seen(&unknown_fixture, 0, &[0x02, 0, 0]),
            Err(ChatStateError::AckValidation)
        ));
        assert_eq!(
            unknown_fixture.tracked_messages,
            unknown_snapshot.tracked_messages
        );
        assert_eq!(original.tracked_messages, snapshot.tracked_messages);
    }

    #[test]
    fn same_session_reset_is_idempotent() {
        let player_id = Uuid::from_u128(0x100);
        let session_id = Uuid::from_u128(0x101);
        let owner = owner();
        assert!(reset_inbound_state(player_id, session_id, &owner));
        {
            let mut store = inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let state = store.states.get_mut(&player_id).expect("installed state");
            state.next_index = 4;
            state.broken = true;
        }

        assert!(reset_inbound_state(player_id, session_id, &owner));

        let state = inbound_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .states
            .get(&player_id)
            .cloned()
            .expect("same-session state remains");
        assert_eq!(state.session_id, session_id);
        assert_eq!(state.next_index, 4);
        assert!(state.broken);
        assert!(clear_inbound_state(player_id, session_id, &owner));
    }

    #[test]
    fn old_disconnect_cannot_clear_new_session_or_duplicate_login_state() {
        let player_id = Uuid::from_u128(0x200);
        let old_session = Uuid::from_u128(0x201);
        let new_session = Uuid::from_u128(0x202);
        let owner = owner();
        assert!(reset_inbound_state(player_id, old_session, &owner));
        assert!(reset_inbound_state(player_id, new_session, &owner));

        assert!(!clear_inbound_state(player_id, old_session, &owner));
        assert_eq!(
            inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .states
                .get(&player_id)
                .map(|state| state.session_id),
            Some(new_session)
        );
        assert!(clear_inbound_state(player_id, new_session, &owner));
    }

    #[test]
    fn old_player_token_cannot_clear_same_session_replacement() {
        let player_id = Uuid::from_u128(0x250);
        let session_id = Uuid::from_u128(0x251);
        let old_owner = owner();
        let new_owner = owner();
        assert!(reset_inbound_state(player_id, session_id, &new_owner));

        assert!(!clear_inbound_state(player_id, session_id, &old_owner));
        assert!(clear_inbound_state(player_id, session_id, &new_owner));
    }

    #[test]
    fn retired_owner_is_replaced_only_after_duplicate_login_publication() {
        let player_id = Uuid::from_u128(0x260);
        let old_session = Uuid::from_u128(0x261);
        let new_session = Uuid::from_u128(0x262);
        let old_owner = owner();
        let new_owner = owner();
        assert!(reset_inbound_state(player_id, old_session, &old_owner));
        old_owner.retire();
        assert!(reset_inbound_state(player_id, new_session, &new_owner));
        assert!(!clear_inbound_state(player_id, old_session, &old_owner));
        assert_eq!(
            inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .states
                .get(&player_id)
                .map(|state| state.session_id),
            Some(new_session)
        );
        assert!(clear_inbound_state(player_id, new_session, &new_owner));
    }

    #[test]
    fn late_retired_owner_cannot_recreate_after_cleanup() {
        let player_id = Uuid::from_u128(0x275);
        let session_id = Uuid::from_u128(0x276);
        let owner = owner();
        assert!(reset_inbound_state(player_id, session_id, &owner));
        owner.retire();
        assert!(clear_inbound_state(player_id, session_id, &owner));
        assert!(!reset_inbound_state(player_id, session_id, &owner));
        assert!(
            !inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .states
                .contains_key(&player_id)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_login_runtime_preserves_new_owner_transfer_and_bounded_state() {
        let temp_world = TempDir::new().expect("temporary runtime world");
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

        let player_id = Uuid::from_u128(0x26_2_0001);
        let profile = GameProfile {
            id: player_id,
            name: "runtime_duplicate".to_string(),
            properties: ArcSwap::from_pointee(Vec::new()),
            profile_actions: None,
        };
        let client = runtime_java_client(&profile).await;
        let listed_config = PlayerConfig {
            server_listing: true,
            ..PlayerConfig::default()
        };
        let (old, old_world) = server
            .add_player(client.clone(), profile.clone(), Some(listed_config.clone()))
            .expect("old runtime player published");
        let old_session = Uuid::from_u128(0x26_2_0002);
        assert!(reset_inbound_state(player_id, old_session, &old.chat_owner));
        old.chat_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session_id = old_session;

        // This is the production Server::add_player -> World::add_player path,
        // not a pure state helper. Publication retires the old Player token.
        let (new, _new_world) = server
            .add_player(client, profile, Some(listed_config))
            .expect("new duplicate runtime player published");
        assert!(!old.chat_owner.is_active());
        let new_session = Uuid::from_u128(0x26_2_0003);
        assert!(reset_inbound_state(player_id, new_session, &new.chat_owner));
        new.chat_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session_id = new_session;
        assert!(!reset_inbound_state(
            player_id,
            old_session,
            &old.chat_owner
        ));

        let signature = [0_u8; 256];
        let late_message = SChatMessage {
            message: "late",
            timestamp: 1,
            salt: 0,
            signature: Some(&signature),
            message_count: VarInt(0),
            acknowledged: &[],
            checksum: 0,
        };
        assert_eq!(
            verify_and_commit(&old, old_session, &[], &late_message),
            Err(ChatStateError::LifecycleMismatch)
        );
        assert_eq!(
            inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .states
                .get(&player_id)
                .map(|state| state.session_id),
            Some(new_session)
        );

        let overworld = server.get_world_from_dimension(&Dimension::OVERWORLD);
        let nether = server.get_world_from_dimension(&Dimension::THE_NETHER);
        // Exercise the same production detach/re-publish boundary used by
        // Player::teleport_world without waiting for a fake socket's chunk ACKs.
        let current_world = new.world();
        current_world
            .remove_player(&new, crate::world::PlayerRemovalReason::DimensionTransfer)
            .await
            .expect("new player detached for transfer");
        new.change_world_chunks(&current_world.level, &nether);
        new.living_entity.entity.set_world(nether.clone());
        nether.players.rcu(|current_list| {
            let mut new_list = (**current_list).clone();
            new_list.push(new.clone());
            new_list
        });
        assert!(Arc::ptr_eq(&new.world(), &nether));
        assert!(reset_inbound_state(player_id, new_session, &new.chat_owner));
        assert!(
            nether
                .players
                .load()
                .iter()
                .any(|candidate| Arc::ptr_eq(candidate, &new))
        );

        old_world
            .remove_player(&old, crate::world::PlayerRemovalReason::Disconnect)
            .await;
        server.remove_player(&old);
        assert!(
            nether
                .players
                .load()
                .iter()
                .any(|candidate| Arc::ptr_eq(candidate, &new))
        );
        assert!(
            overworld
                .entity_tracker
                .entity_map
                .get(&old.entity_id())
                .is_none()
        );
        let listed_after_old_disconnect = server
            .get_status()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status_response
            .players
            .as_ref()
            .expect("status players")
            .online;
        assert_eq!(listed_after_old_disconnect, 1);

        let new_world = new.world();
        new_world
            .remove_player(&new, crate::world::PlayerRemovalReason::Disconnect)
            .await;
        server.remove_player(&new);
        assert!(
            !inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .states
                .contains_key(&player_id)
        );
        let listed_after_new_disconnect = server
            .get_status()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status_response
            .players
            .as_ref()
            .expect("status players")
            .online;
        assert_eq!(listed_after_new_disconnect, 0);

        // Repeated real Player/World/CachedStatus publication and disconnect
        // must not leave one retired entry per connection in the global store.
        for _ in 0..32 {
            let profile = GameProfile {
                id: player_id,
                name: "runtime_duplicate".to_string(),
                properties: ArcSwap::from_pointee(Vec::new()),
                profile_actions: None,
            };
            let (cycle, cycle_world) = server
                .add_player(
                    runtime_java_client(&profile).await,
                    profile,
                    Some(PlayerConfig {
                        server_listing: true,
                        ..PlayerConfig::default()
                    }),
                )
                .expect("bounded-cycle player published");
            let session = Uuid::new_v4();
            assert!(reset_inbound_state(player_id, session, &cycle.chat_owner));
            cycle
                .chat_session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .session_id = session;
            cycle_world
                .remove_player(&cycle, crate::world::PlayerRemovalReason::Disconnect)
                .await;
            server.remove_player(&cycle);
            assert!(
                !inbound_state()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .states
                    .contains_key(&player_id)
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn signed_command_fixture_exercises_n2_chain_progress_and_no_signable_ack() {
        const FIXTURE: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tools/compat-26_2/chat-fixtures/signed-command-independent.json"
        ));
        let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture JSON");
        let public_key = fixture_bytes(
            fixture["crypto"]["public_key_der_hex"]
                .as_str()
                .expect("fixture public key"),
        );
        let vector = fixture["vectors"]
            .as_array()
            .expect("fixture vectors")
            .iter()
            .find(|vector| vector["n"].as_u64() == Some(2))
            .expect("N=2 fixture");
        let packet = &vector["packet"];
        let arguments = vector["arguments"].as_array().expect("fixture arguments");
        let contents = arguments
            .iter()
            .map(|argument| {
                argument["raw_value_text"]
                    .as_str()
                    .expect("fixture raw value")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        let signatures = arguments
            .iter()
            .map(|argument| {
                fixture_bytes(
                    argument["signature_hex"]
                        .as_str()
                        .expect("fixture signature"),
                )
            })
            .collect::<Vec<_>>();
        let signature_refs = signatures.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let content_refs = contents.iter().map(String::as_str).collect::<Vec<_>>();
        let acknowledged = [0x05, 0, 0];
        let session_id = Uuid::from_u128(2);
        let player_id = Uuid::from_u128(1);

        let temp_world = TempDir::new().expect("temporary runtime world");
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
        let profile = GameProfile {
            id: player_id,
            name: "signed_command_fixture".to_owned(),
            properties: ArcSwap::from_pointee(Vec::new()),
            profile_actions: None,
        };
        let (player, world) = server
            .add_player(
                runtime_java_client(&profile).await,
                profile,
                Some(PlayerConfig::default()),
            )
            .expect("fixture player published");
        *player
            .chat_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ChatSession::new(
            session_id,
            i64::MAX,
            public_key.clone().into_boxed_slice(),
            vec![1].into_boxed_slice(),
        );
        assert!(reset_inbound_state(
            player_id,
            session_id,
            &player.chat_owner
        ));
        {
            let mut store = inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            store
                .states
                .get_mut(&player_id)
                .expect("fixture state")
                .next_index = 41;
        }

        let zero = vec![0_u8; 256].into_boxed_slice();
        let eighty = vec![0x80_u8; 256].into_boxed_slice();
        let mut cache = player
            .signature_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.last_seen_validator = LastSeenMessagesValidator::new(3);
        cache.last_seen_validator.tracked_messages = [
            Some(LastSeenTrackedEntry {
                signature: zero,
                pending: true,
            }),
            Some(LastSeenTrackedEntry {
                signature: vec![1_u8; 256].into_boxed_slice(),
                pending: true,
            }),
            Some(LastSeenTrackedEntry {
                signature: eighty,
                pending: true,
            }),
        ]
        .into_iter()
        .collect();
        drop(cache);

        let timestamp = packet["timestamp_epoch_second"]
            .as_i64()
            .expect("fixture timestamp")
            * 1_000;
        let salt = i64::from_str_radix(
            packet["salt_i64"]
                .as_str()
                .expect("fixture salt")
                .trim_start_matches("0x"),
            16,
        )
        .expect("fixture salt hex");
        assert_eq!(
            verify_signed_command_and_commit(
                &player,
                session_id,
                &public_key,
                timestamp,
                salt,
                0,
                &acknowledged,
                0,
                &content_refs,
                &signature_refs,
            ),
            Ok(())
        );
        assert_eq!(
            inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .states
                .get(&player_id)
                .expect("committed state")
                .next_index,
            43
        );

        {
            let mut store = inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let state = store.states.get_mut(&player_id).expect("fixture state");
            state.next_index = 41;
            state.broken = false;
        }
        // Reset the ACK window so the following failure independently proves
        // ACK-first ordering rather than reusing the successful N=2 call.
        {
            let mut cache = player
                .signature_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.last_seen_validator = LastSeenMessagesValidator::new(3);
            cache.last_seen_validator.tracked_messages = [
                Some(LastSeenTrackedEntry {
                    signature: vec![0_u8; 256].into_boxed_slice(),
                    pending: true,
                }),
                Some(LastSeenTrackedEntry {
                    signature: vec![1_u8; 256].into_boxed_slice(),
                    pending: true,
                }),
                Some(LastSeenTrackedEntry {
                    signature: vec![0x80_u8; 256].into_boxed_slice(),
                    pending: true,
                }),
            ]
            .into_iter()
            .collect();
        }
        let mut invalid_signatures = signatures.clone();
        invalid_signatures[1][0] ^= 1;
        let invalid_refs = invalid_signatures
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>();
        assert!(matches!(
            verify_signed_command_and_commit(
                &player,
                session_id,
                &public_key,
                timestamp,
                salt,
                0,
                &acknowledged,
                0,
                &content_refs,
                &invalid_refs,
            ),
            Err(ChatStateError::Signature(_))
        ));
        let cache = player
            .signature_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            cache
                .last_seen_validator
                .tracked_messages
                .iter()
                .map(|entry| entry.as_ref().map(|entry| entry.pending))
                .collect::<Vec<_>>(),
            vec![Some(false), None, Some(false)]
        );
        drop(cache);
        let state = inbound_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .states
            .get(&player_id)
            .cloned()
            .expect("failed state");
        // Vanilla advances the first link before the second signature fails.
        assert_eq!(state.next_index, 42);
        assert_eq!(state.last_timestamp_epoch_second, Some(timestamp / 1_000));
        assert!(state.broken);

        {
            let mut store = inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let state = store.states.get_mut(&player_id).expect("fixture state");
            state.next_index = 41;
            state.broken = false;
        }
        assert_eq!(
            verify_signed_command_and_commit(
                &player,
                session_id,
                &public_key,
                timestamp,
                salt,
                0,
                &acknowledged,
                0,
                &[],
                &[],
            ),
            Ok(())
        );
        assert_eq!(
            inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .states
                .get(&player_id)
                .expect("ack-only state")
                .next_index,
            41
        );

        world
            .remove_player(&player, crate::world::PlayerRemovalReason::Disconnect)
            .await;
        server.remove_player(&player);
    }

    #[test]
    fn broken_chain_is_removed_by_matching_disconnect_cleanup() {
        let player_id = Uuid::from_u128(0x300);
        let session_id = Uuid::from_u128(0x301);
        let owner = owner();
        assert!(reset_inbound_state(player_id, session_id, &owner));
        inbound_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .states
            .get_mut(&player_id)
            .expect("installed state")
            .broken = true;

        assert!(clear_inbound_state(player_id, session_id, &owner));
        assert!(
            !inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .states
                .contains_key(&player_id)
        );
    }
}
