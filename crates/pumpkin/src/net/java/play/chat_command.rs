#[allow(clippy::wildcard_imports)]
use super::*;
use crate::command::{CommandSource, dispatcher::CommandDispatcher};
use pumpkin_command::context::command_context::CommandSigningContext;
use pumpkin_command::node::dispatcher::{ParsingResult, SignableArgument};
use pumpkin_protocol::java::server::play::SChatCommandSigned;
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct SignedCommandArgument {
    pub name: String,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct SignedCommandPacket {
    pub command: String,
    pub timestamp: i64,
    pub salt: i64,
    pub argument_signatures: Vec<SignedCommandArgument>,
    pub message_count: i32,
    pub acknowledged: [u8; 3],
    pub checksum: u8,
}

impl From<&SChatCommandSigned<'_>> for SignedCommandPacket {
    fn from(packet: &SChatCommandSigned<'_>) -> Self {
        Self {
            command: packet.command.to_owned(),
            timestamp: packet.timestamp,
            salt: packet.salt,
            argument_signatures: packet
                .argument_signatures
                .iter()
                .map(|argument| SignedCommandArgument {
                    name: argument.name.to_owned(),
                    signature: argument.signature.to_vec(),
                })
                .collect(),
            message_count: packet.message_count.0,
            acknowledged: packet
                .acknowledged
                .try_into()
                .expect("signed command acknowledgements are fixed-width"),
            checksum: packet.checksum,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SignedCommandArgumentError {
    Malformed,
    Unknown(String),
    Missing(String),
}

fn signed_command_event_matches(expected: &str, actual: &str) -> bool {
    expected == actual
}

fn signed_session(player: &Arc<Player>) -> Option<(uuid::Uuid, Vec<u8>)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let chat_session = player
        .chat_session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if chat_session.session_id == uuid::Uuid::nil()
        || chat_session.public_key.is_empty()
        || chat_session.signature.is_empty()
        || chat_session.expires_at < now
    {
        warn!(
            player = %player.gameprofile.name,
            "Rejected signed command without a valid chat session"
        );
        return None;
    }
    Some((chat_session.session_id, chat_session.public_key.to_vec()))
}

fn validate_signed_command_arguments(
    parsed: Option<&[SignableArgument]>,
    signatures: &[SignedCommandArgument],
) -> Result<(), SignedCommandArgumentError> {
    let Some(parsed) = parsed else {
        return Err(SignedCommandArgumentError::Malformed);
    };

    let mut received = HashSet::with_capacity(signatures.len());
    for signature in signatures {
        if !parsed
            .iter()
            .any(|argument| argument.name == signature.name)
        {
            return Err(SignedCommandArgumentError::Unknown(signature.name.clone()));
        }
        // Vanilla permits duplicate entries: each one is decoded in packet
        // order and the map value is replaced by the last entry.
        received.insert(signature.name.as_str());
    }

    for argument in parsed {
        if !received.contains(argument.name.as_str()) {
            return Err(SignedCommandArgumentError::Missing(argument.name.clone()));
        }
    }

    Ok(())
}

impl JavaClient {
    pub async fn handle_chat_command(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        command: &SChatCommand<'_>,
    ) {
        if server.basic_config.allow_chat_reports {
            let source = player.get_command_source(server);
            let dispatcher = server.command_dispatcher.load();
            if dispatcher
                .parse_input(command.command, &source)
                .signable_arguments()
                .is_some_and(|arguments| !arguments.is_empty())
            {
                warn!(
                    player = %player.gameprofile.name,
                    "Rejected unsigned command with signable arguments"
                );
                return;
            }
        }
        self.execute_chat_command(player, server, command).await;
    }

    async fn execute_chat_command(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        command: &SChatCommand<'_>,
    ) {
        player.update_last_action_time();
        if player.check_chat_spam(server, crate::entity::player::SpamType::Command) {
            return;
        }
        let command_str = command.command.strip_prefix('/').unwrap_or(command.command);
        send_cancellable! {{
            server;
            PlayerCommandSendEvent {
                player: player.clone(),
                command: command_str.to_string(),
                cancelled: false
            };

            'after: {
                let command = event.command;
                let dispatcher = server.command_dispatcher.load();
                dispatcher.handle_command(
                    &player.get_command_source(server),
                    &command,
                );

                if server.advanced_config.commands.log_console {
                    info!(
                        "Player ({}): executed command /{}",
                        player.gameprofile.name,
                        command
                    );
                }
            }
        }}
    }

    fn execute_parsed_chat_command<'a>(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        command: &str,
        dispatcher: &'a CommandDispatcher,
        parsed: ParsingResult<'a, CommandSource>,
        signing_context: Arc<CommandSigningContext>,
    ) {
        player.update_last_action_time();
        if player.check_chat_spam(server, crate::entity::player::SpamType::Command) {
            return;
        }
        let expected = command.strip_prefix('/').unwrap_or(command);
        let mut event = PlayerCommandSendEvent::new(player.clone(), expected.to_owned());
        server.plugin_manager.fire_blocking(server, &mut event);
        if event.cancelled {
            return;
        }
        if !signed_command_event_matches(expected, &event.command) {
            warn!(
                player = %player.gameprofile.name,
                original = %expected,
                mutated = %event.command,
                "Rejected signed command after command event mutation"
            );
            return;
        }

        let command = event.command;
        // Signed and unsigned-decoder paths must execute the parse result that
        // was already permission-filtered and validated above. Re-parsing here
        // would let command-tree changes alter the selected execution path.
        if let Err(error) = dispatcher.execute_with_signing_context(parsed, signing_context) {
            CommandDispatcher::send_error_to_source(
                &player.get_command_source(server),
                error,
                &command,
            );
        }
        if server.advanced_config.commands.log_console {
            info!(
                "Player ({}): executed command /{}",
                player.gameprofile.name, command
            );
        }
    }

    pub fn handle_signed_chat_command(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        packet: SignedCommandPacket,
    ) {
        let _chat_lifecycle = player
            .chat_lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Vanilla applies ACKs before parsing or signed-argument decoding.
        let last_seen = match crate::net::chat::state::apply_last_seen_update(
            player,
            packet.message_count,
            &packet.acknowledged,
            packet.checksum,
        ) {
            Ok(last_seen) => last_seen,
            Err(error) => {
                warn!(
                    player = %player.gameprofile.name,
                    ?error,
                    "Rejected signed command acknowledgement"
                );
                self.try_kick(&TextComponent::translate_cross(
                    translation::java::MULTIPLAYER_DISCONNECT_CHAT_VALIDATION_FAILED,
                    translation::java::MULTIPLAYER_DISCONNECT_CHAT_VALIDATION_FAILED,
                    [],
                ));
                return;
            }
        };

        let source = player.get_command_source(server);
        let dispatcher = server.command_dispatcher.load();
        let parsed = dispatcher.parse_input(&packet.command, &source);
        let Some(signable) = parsed.signable_arguments() else {
            warn!(
                player = %player.gameprofile.name,
                "Rejected signed command with malformed or permission-filtered parse"
            );
            return;
        };
        let secure = server.basic_config.allow_chat_reports;
        let has_entries = !packet.argument_signatures.is_empty();

        // An empty signed-argument list is vanilla's unsigned seam. It is
        // legal for commands with no signable arguments in every mode, and for
        // signable commands when secure-profile enforcement is disabled.
        if !has_entries {
            if secure && !signable.is_empty() {
                warn!(
                    player = %player.gameprofile.name,
                    "Rejected unsigned signable command while secure chat is enabled"
                );
                return;
            }
            let signing_context = if signable.is_empty() {
                CommandSigningContext::empty()
            } else {
                CommandSigningContext::from_unsigned(&signable, &packet.command)
            };
            self.execute_parsed_chat_command(
                player,
                server,
                &packet.command,
                &dispatcher,
                parsed,
                Arc::new(signing_context),
            );
            return;
        }

        let first_unknown = packet
            .argument_signatures
            .iter()
            .position(|entry| !signable.iter().any(|argument| argument.name == entry.name));

        // Vanilla rejects an unknown name and marks a secure chain broken. In
        // reports-disabled mode its unsigned decoder has a no-op break marker.
        if let Some(unknown_index) = first_unknown {
            if !secure {
                return;
            }
            if unknown_index > 0 {
                let (session_id, public_key) = match signed_session(player) {
                    Some(session) => session,
                    None => return,
                };
                let mut contents = Vec::with_capacity(unknown_index);
                let mut signatures = Vec::with_capacity(unknown_index);
                for entry in &packet.argument_signatures[..unknown_index] {
                    let argument = signable
                        .iter()
                        .find(|argument| argument.name == entry.name)
                        .expect("unknown entry is after known prefix");
                    contents.push(argument.raw_value(&packet.command));
                    signatures.push(entry.signature.as_slice());
                }
                if let Err(error) = crate::net::chat::state::verify_signed_command_entries(
                    player,
                    session_id,
                    &public_key,
                    packet.timestamp,
                    packet.salt,
                    &last_seen,
                    &contents,
                    &signatures,
                ) {
                    warn!(
                        player = %player.gameprofile.name,
                        ?error,
                        "Rejected signed command before unknown argument entry"
                    );
                    return;
                }
            }
            crate::net::chat::state::break_inbound_chain(player);
            return;
        }

        let mut contents = Vec::with_capacity(packet.argument_signatures.len());
        let mut signatures = Vec::with_capacity(packet.argument_signatures.len());
        for entry in &packet.argument_signatures {
            let argument = signable
                .iter()
                .find(|argument| argument.name == entry.name)
                .expect("unknown entries were handled above");
            contents.push(argument.raw_value(&packet.command));
            signatures.push(entry.signature.as_slice());
        }

        if secure {
            let (session_id, public_key) = match signed_session(player) {
                Some(session) => session,
                None => return,
            };
            if let Err(error) = crate::net::chat::state::verify_signed_command_entries(
                player,
                session_id,
                &public_key,
                packet.timestamp,
                packet.salt,
                &last_seen,
                &contents,
                &signatures,
            ) {
                warn!(
                    player = %player.gameprofile.name,
                    ?error,
                    "Rejected signed command authentication"
                );
                return;
            }
        }

        // Missing is checked after packet-entry decoding. This preserves the
        // vanilla partial chain advance and does not mark the chain broken.
        if let Err(error) =
            validate_signed_command_arguments(Some(&signable), &packet.argument_signatures)
        {
            warn!(
                player = %player.gameprofile.name,
                ?error,
                "Rejected signed command argument set"
            );
            return;
        }

        let signing_context = if secure {
            CommandSigningContext::from_signed_entries(
                &signable,
                &packet.command,
                packet
                    .argument_signatures
                    .iter()
                    .map(|entry| (entry.name.as_str(), entry.signature.as_slice())),
            )
            .expect("validated signed command entries must map to parsed arguments")
        } else {
            // In reports-disabled mode entries are decoded as unsigned messages;
            // their bytes are deliberately not treated as authenticated.
            CommandSigningContext::from_unsigned(&signable, &packet.command)
        };
        self.execute_parsed_chat_command(
            player,
            server,
            &packet.command,
            &dispatcher,
            parsed,
            Arc::new(signing_context),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SignedCommandArgument, SignedCommandArgumentError, signed_command_event_matches,
        validate_signed_command_arguments,
    };
    use arc_swap::ArcSwap;
    use pumpkin_command::context::string_range::StringRange;
    use pumpkin_command::node::attached::NodeId;
    use pumpkin_command::node::dispatcher::SignableArgument;
    use pumpkin_config::{AdvancedConfiguration, BasicConfiguration, TelemetryConfig};
    use pumpkin_protocol::codec::var_int::VarInt;
    use rand::SeedableRng;
    use rsa::RsaPrivateKey;
    use rsa::pkcs8::EncodePublicKey;
    use rsa::signature::{SignatureEncoding, Signer};
    use sha2::Sha256;
    use std::net::SocketAddr;
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::net::{TcpListener, TcpStream};
    use uuid::Uuid;

    fn parsed(name: &str) -> SignableArgument {
        SignableArgument {
            name: name.to_owned(),
            value: StringRange::between(0, 1),
            node: NodeId(NonZeroUsize::new(2).expect("non-zero node id")),
        }
    }

    fn signature(name: &str) -> SignedCommandArgument {
        SignedCommandArgument {
            name: name.to_owned(),
            signature: vec![0; 256],
        }
    }

    #[test]
    fn signed_event_cannot_replace_authenticated_command() {
        assert!(signed_command_event_matches("demo safe", "demo safe"));
        assert!(!signed_command_event_matches("demo safe", "op"));
    }

    #[test]
    fn signed_argument_sets_reject_malformed_unknown_duplicate_and_missing() {
        assert_eq!(
            validate_signed_command_arguments(None, &[]),
            Err(SignedCommandArgumentError::Malformed)
        );
        assert_eq!(
            validate_signed_command_arguments(Some(&[parsed("message")]), &[signature("other")]),
            Err(SignedCommandArgumentError::Unknown("other".into()))
        );
        assert_eq!(
            validate_signed_command_arguments(
                Some(&[parsed("message")]),
                &[signature("message"), signature("message")]
            ),
            Ok(())
        );
        assert_eq!(
            validate_signed_command_arguments(Some(&[parsed("message")]), &[]),
            Err(SignedCommandArgumentError::Missing("message".into()))
        );
    }

    #[test]
    fn empty_signable_set_is_a_valid_unsigned_seam() {
        assert_eq!(validate_signed_command_arguments(Some(&[]), &[]), Ok(()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn signed_command_handler_applies_ack_verifies_argument_and_advances_chat_chain() {
        use crate::entity::player::{ChatSession, LastSeenMessagesValidator, LastSeenTrackedEntry};
        use crate::net::chat::signature::canonical_bytes;
        use crate::net::chat::state::{reset_inbound_state, verify_and_commit};
        use crate::net::chat::{SignedMessageBody, SignedMessageLink};
        use crate::net::java::JavaClient;
        use crate::net::{ClientPlatform, GameProfile, PacketRateLimiter, PlayerConfig};

        async fn runtime_client(profile: &GameProfile) -> Arc<ClientPlatform> {
            let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .expect("fixture listener");
            let address = listener.local_addr().expect("listener address");
            let connector = tokio::spawn(TcpStream::connect(address));
            let (stream, peer) = listener.accept().await.expect("fixture accept");
            connector.await.expect("connector task").expect("connect");
            let pending = crate::net::java::pending::PendingConnection::new(
                stream,
                peer,
                1,
                PacketRateLimiter::new(false, 0.0, 0.0),
            );
            Arc::new(ClientPlatform::Java(JavaClient::from_pending(
                pending,
                profile.clone(),
                PlayerConfig::default(),
            )))
        }

        let mut rng = rand::rngs::StdRng::seed_from_u64(0x12_26_02);
        let private_key = RsaPrivateKey::new(&mut rng, 2048).expect("test RSA key");
        let public_key = private_key
            .to_public_key()
            .to_public_key_der()
            .expect("test public key DER")
            .as_bytes()
            .to_vec();
        let signing_key = rsa::pkcs1v15::SigningKey::<Sha256>::new(private_key);
        let player_id = Uuid::from_u128(0x12_2602_01);
        let session_id = Uuid::from_u128(0x12_2602_02);

        let temp_world = TempDir::new().expect("temporary world");
        let mut basic = BasicConfiguration::default();
        basic.default_level_name = temp_world.path().to_string_lossy().into_owned();
        basic.allow_chat_reports = true;
        basic.allow_nether = true;
        basic.allow_end = false;
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
        let vanilla = crate::data::VanillaData {
            banned_ip_list: std::sync::RwLock::new(Default::default()),
            banned_player_list: std::sync::RwLock::new(Default::default()),
            operator_config: std::sync::RwLock::new(Default::default()),
            user_cache: std::sync::RwLock::new(Default::default()),
            whitelist_config: std::sync::RwLock::new(Default::default()),
        };
        let server = crate::server::Server::new(
            basic,
            advanced,
            TelemetryConfig {
                enabled: false,
                ..TelemetryConfig::default()
            },
            vanilla,
        )
        .await;
        let profile = GameProfile {
            id: player_id,
            name: "signed_command_handler_test".to_owned(),
            properties: ArcSwap::from_pointee(Vec::new()),
            profile_actions: None,
        };
        let (player, world) = server
            .add_player(
                runtime_client(&profile).await,
                profile,
                Some(PlayerConfig::default()),
            )
            .expect("test player published");
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

        let ack_signatures = [0x41_u8, 0x42, 0x43].map(|byte| vec![byte; 256]);
        {
            let mut cache = player
                .signature_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.last_seen_validator = LastSeenMessagesValidator::new(3);
            cache.last_seen_validator.tracked_messages = ack_signatures
                .iter()
                .map(|signature| {
                    Some(LastSeenTrackedEntry {
                        signature: signature.clone().into_boxed_slice(),
                        pending: true,
                    })
                })
                .collect();
        }

        player
            .permission_lvl
            .store(pumpkin_util::PermissionLvl::Four);
        let acknowledged_signatures = ack_signatures
            .iter()
            .cloned()
            .map(Vec::into_boxed_slice)
            .collect::<Vec<_>>();
        let timestamp = 1_700_000_000_000;
        let salt = 0x12_34;
        let sign = |index, content: &str, timestamp, salt| {
            let link = SignedMessageLink::new(index, player_id, session_id);
            let body = SignedMessageBody::new(
                content.to_owned(),
                timestamp,
                salt,
                acknowledged_signatures.clone(),
            );
            signing_key
                .sign(&canonical_bytes(&link, &body).expect("canonical test payload"))
                .to_vec()
        };
        let command_signature = sign(0, "hello", timestamp, salt);
        let client = match player.client.as_ref() {
            ClientPlatform::Java(client) => client,
            ClientPlatform::Bedrock(_) => unreachable!("test uses a Java client"),
        };
        client.handle_signed_chat_command(
            &player,
            &server,
            super::SignedCommandPacket {
                command: "say hello".to_owned(),
                timestamp,
                salt,
                argument_signatures: vec![SignedCommandArgument {
                    name: "message".to_owned(),
                    signature: command_signature,
                }],
                message_count: 0,
                acknowledged: [0x07, 0, 0],
                checksum: 0,
            },
        );

        assert_eq!(
            player
                .signature_cache
                .lock()
                .unwrap()
                .last_seen_validator
                .tracked_messages
                .iter()
                .map(|entry| entry.as_ref().map(|entry| entry.pending))
                .collect::<Vec<_>>(),
            vec![Some(false), Some(false), Some(false)]
        );
        let next_signature = sign(1, "after", timestamp + 1_000, salt + 1);
        assert_eq!(
            verify_and_commit(
                &player,
                session_id,
                &public_key,
                &pumpkin_protocol::java::server::play::SChatMessage {
                    message: "after",
                    timestamp: timestamp + 1_000,
                    salt: salt + 1,
                    signature: Some(&next_signature),
                    message_count: VarInt(0),
                    acknowledged: &[0x07, 0, 0],
                    checksum: 0,
                },
            ),
            Ok(())
        );

        world
            .remove_player(&player, crate::world::PlayerRemovalReason::Disconnect)
            .await;
        server.remove_player(&player);
    }
}
