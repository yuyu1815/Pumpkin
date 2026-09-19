//! Transactional state for inbound 26.2 secure-chat messages.
//!
//! `ChatSession` currently does not own the inbound chain cursor, and changing
//! the player entity is outside this task's ownership boundary. This module
//! therefore keeps the small inbound cursor in a UUID-keyed sidecar. Ack state
//! is never mutated until canonical signature verification succeeds.

use std::collections::{HashMap, hash_map::Entry};
use std::sync::{Mutex, OnceLock};

use pumpkin_protocol::java::server::play::SChatMessage;
use thiserror::Error;
use uuid::Uuid;

use crate::entity::player::{LastSeenMessagesValidator, Player};

use super::signature::{ChatSignatureError, verify_chat_message_signature};
use super::{SignedMessageBody, SignedMessageLink};

const ACK_UPDATE_LEN: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
struct InboundChatState {
    session_id: Uuid,
    /// Identity of the Player allocation that most recently committed this
    /// session. This is a connection generation without changing Player.
    owner_generation: Option<u64>,
    next_index: i32,
    last_timestamp_epoch_millis: Option<i64>,
    broken: bool,
}

impl InboundChatState {
    fn new(session_id: Uuid) -> Self {
        Self {
            session_id,
            owner_generation: None,
            next_index: 0,
            last_timestamp_epoch_millis: None,
            broken: false,
        }
    }

    fn check_link(
        &self,
        session_id: Uuid,
        index: i32,
        timestamp_epoch_second: i64,
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
            .is_some_and(|last| timestamp_epoch_second < last)
        {
            return Err(ChatStateError::OutOfOrderTimestamp);
        }
        Ok(())
    }

    fn advance(&mut self, timestamp_epoch_second: i64) -> Result<(), ChatStateError> {
        self.next_index = self
            .next_index
            .checked_add(1)
            .ok_or(ChatStateError::IndexOverflow)?;
        self.last_timestamp_epoch_millis = Some(timestamp_epoch_second);
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
    states: HashMap<Uuid, InboundChatState>,
    /// The highest retired Player generation for each UUID. Keeping this tombstone
    /// prevents a delayed task from recreating state after disconnect cleanup.
    retired_generations: HashMap<Uuid, u64>,
}

static INBOUND_STATE: OnceLock<Mutex<InboundStateStore>> = OnceLock::new();

fn inbound_state() -> &'static Mutex<InboundStateStore> {
    INBOUND_STATE.get_or_init(|| Mutex::new(InboundStateStore::default()))
}

/// Installs the inbound cursor after the existing session certificate checks pass.
///
/// Repeating the exact same session update is deliberately a no-op. The explicit
/// Player generation is the lifecycle owner; a delayed old connection cannot
/// replace a newer owner, and a retired owner cannot recreate a cleared state.
pub(crate) fn reset_inbound_state(
    player_id: Uuid,
    session_id: Uuid,
    owner_generation: u64,
) -> bool {
    let mut store = inbound_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if store
        .retired_generations
        .get(&player_id)
        .is_some_and(|retired| owner_generation <= *retired)
    {
        return false;
    }
    match store.states.entry(player_id) {
        Entry::Occupied(mut entry) => {
            let state = entry.get_mut();
            if let Some(current_owner) = state.owner_generation
                && owner_generation < current_owner
            {
                return false;
            }
            if state.owner_generation == Some(owner_generation) && state.session_id == session_id {
                return true;
            }
            *state = InboundChatState {
                session_id,
                owner_generation: Some(owner_generation),
                ..InboundChatState::new(session_id)
            };
        }
        Entry::Vacant(entry) => {
            let mut state = InboundChatState::new(session_id);
            state.owner_generation = Some(owner_generation);
            entry.insert(state);
        }
    }
    true
}

/// Retires a disconnect owner and clears only its matching installed session.
///
/// The retirement tombstone and removal are one state-store operation. This is
/// the ABA boundary: after this returns, a late task from this Player generation
/// cannot recreate or mutate the UUID-keyed sidecar.
pub(crate) fn clear_inbound_state(
    player_id: Uuid,
    session_id: Uuid,
    owner_generation: u64,
) -> bool {
    let mut store = inbound_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    store
        .retired_generations
        .entry(player_id)
        .and_modify(|retired| *retired = (*retired).max(owner_generation))
        .or_insert(owner_generation);
    if store.states.get(&player_id).is_some_and(|state| {
        state.session_id == session_id && state.owner_generation == Some(owner_generation)
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
    let player_id = player.gameprofile.id;
    let timestamp_epoch_millis = chat_message.timestamp;
    let signature = chat_message
        .signature
        .ok_or(ChatSignatureError::InvalidSignatureLength {
            length: 0,
            expected: super::signature::CHAT_SIGNATURE_LEN,
        })?;

    let mut store = inbound_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(mut state) = store.states.get(&player_id).cloned() else {
        return Err(ChatStateError::LifecycleMismatch);
    };
    if state.owner_generation != Some(player.chat_owner_generation)
        || state.session_id != session_id
    {
        return Err(ChatStateError::LifecycleMismatch);
    }
    if let Err(error) = state.check_link(session_id, state.next_index, timestamp_epoch_millis) {
        if matches!(
            &error,
            ChatStateError::Replay { .. }
                | ChatStateError::OutOfOrderTimestamp
                | ChatStateError::SessionMismatch
        ) {
            state.broken = true;
            store.states.insert(player_id, state);
        }
        return Err(error);
    }

    let mut cache = player
        .signature_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (validator, acknowledged) = preview_last_seen(
        &cache.last_seen_validator,
        chat_message.message_count.0,
        chat_message.acknowledged,
    )?;

    if validator.tracked_messages_count() > 4096 {
        return Err(ChatStateError::TooManyPendingChats);
    }

    if chat_message.checksum != 0 {
        let expected = last_seen_checksum(&acknowledged);
        if expected != chat_message.checksum {
            return Err(ChatStateError::ChecksumMismatch {
                expected,
                received: chat_message.checksum,
            });
        }
    }

    let link = SignedMessageLink::new(state.next_index, player_id, session_id);
    let body = SignedMessageBody::new(
        chat_message.message.to_string(),
        chat_message.timestamp,
        chat_message.salt,
        acknowledged,
    );
    if let Err(error) = verify_chat_message_signature(public_key, &link, &body, signature) {
        state.broken = true;
        store.states.insert(player_id, state);
        return Err(error.into());
    }

    state.advance(timestamp_epoch_millis)?;
    cache.last_seen_validator = validator;
    store.states.insert(player_id, state);
    Ok(())
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
        ChatStateError, InboundChatState, clear_inbound_state, inbound_state, last_seen_checksum,
        preview_last_seen, reset_inbound_state,
    };
    use crate::entity::player::{LastSeenMessagesValidator, LastSeenTrackedEntry};
    use crate::net::chat::signature::canonical_bytes;
    use crate::net::chat::{SignedMessageBody, SignedMessageLink};
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    fn state() -> InboundChatState {
        InboundChatState::new(Uuid::from_u128(10))
    }

    #[test]
    fn root_and_next_indices_advance_only_after_success() {
        let mut state = state();
        assert_eq!(state.check_link(Uuid::from_u128(10), 0, 1), Ok(()));
        state.advance(1).expect("root commit");
        assert_eq!(state.next_index, 1);
        assert_eq!(state.last_timestamp_epoch_millis, Some(1));
        assert_eq!(
            state.check_link(Uuid::from_u128(10), 0, 2),
            Err(ChatStateError::Replay {
                expected: 1,
                received: 0,
            })
        );
        assert_eq!(state.next_index, 1);
        assert_eq!(state.last_timestamp_epoch_millis, Some(1));
    }

    #[test]
    fn old_timestamp_is_separate_and_does_not_advance() {
        let mut state = state();
        state.advance(1_000).expect("root commit");
        assert_eq!(
            state.check_link(Uuid::from_u128(10), 1, 999),
            Err(ChatStateError::OutOfOrderTimestamp)
        );
        assert_eq!(state.next_index, 1);
        assert_eq!(state.last_timestamp_epoch_millis, Some(1_000));
    }

    #[test]
    fn chain_break_is_sticky_after_replay_or_signature_failure() {
        let mut state = state();
        state.advance(1_000).expect("root commit");
        state.broken = true;
        assert_eq!(
            state.check_link(Uuid::from_u128(10), 1, 1_001),
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
        assert!(reset_inbound_state(player_id, session_id, 1));
        {
            let mut store = inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let state = store.states.get_mut(&player_id).expect("installed state");
            state.next_index = 4;
            state.broken = true;
        }

        assert!(reset_inbound_state(player_id, session_id, 1));

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
        assert!(clear_inbound_state(player_id, session_id, 1));
    }

    #[test]
    fn old_disconnect_cannot_clear_new_session_or_duplicate_login_state() {
        let player_id = Uuid::from_u128(0x200);
        let old_session = Uuid::from_u128(0x201);
        let new_session = Uuid::from_u128(0x202);
        assert!(reset_inbound_state(player_id, old_session, 1));
        assert!(reset_inbound_state(player_id, new_session, 1));

        assert!(!clear_inbound_state(player_id, old_session, 1));
        assert_eq!(
            inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .states
                .get(&player_id)
                .map(|state| state.session_id),
            Some(new_session)
        );
        assert!(clear_inbound_state(player_id, new_session, 1));
    }

    #[test]
    fn old_player_generation_cannot_clear_same_session_replacement() {
        let player_id = Uuid::from_u128(0x250);
        let session_id = Uuid::from_u128(0x251);
        assert!(reset_inbound_state(player_id, session_id, 22));

        assert!(!clear_inbound_state(player_id, session_id, 11));
        assert!(clear_inbound_state(player_id, session_id, 22));
    }

    #[test]
    fn newer_player_generation_replaces_old_state() {
        let player_id = Uuid::from_u128(0x260);
        let old_session = Uuid::from_u128(0x261);
        let new_session = Uuid::from_u128(0x262);
        assert!(reset_inbound_state(player_id, old_session, 11));
        assert!(reset_inbound_state(player_id, new_session, 22));
        assert!(!clear_inbound_state(player_id, old_session, 11));
        assert_eq!(
            inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .states
                .get(&player_id)
                .map(|state| (state.session_id, state.owner_generation)),
            Some((new_session, Some(22)))
        );
        assert!(clear_inbound_state(player_id, new_session, 22));
    }

    #[test]
    fn late_old_generation_cannot_recreate_after_cleanup() {
        let player_id = Uuid::from_u128(0x275);
        let session_id = Uuid::from_u128(0x276);
        assert!(reset_inbound_state(player_id, session_id, 11));
        assert!(clear_inbound_state(player_id, session_id, 11));
        assert!(!reset_inbound_state(player_id, session_id, 11));
        assert!(
            !inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .states
                .contains_key(&player_id)
        );
    }

    #[test]
    fn broken_chain_is_removed_by_matching_disconnect_cleanup() {
        let player_id = Uuid::from_u128(0x300);
        let session_id = Uuid::from_u128(0x301);
        assert!(reset_inbound_state(player_id, session_id, 1));
        inbound_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .states
            .get_mut(&player_id)
            .expect("installed state")
            .broken = true;

        assert!(clear_inbound_state(player_id, session_id, 1));
        assert!(
            !inbound_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .states
                .contains_key(&player_id)
        );
    }
}
