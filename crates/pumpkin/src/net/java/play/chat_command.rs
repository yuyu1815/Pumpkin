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
    use pumpkin_command::context::string_range::StringRange;
    use pumpkin_command::node::attached::NodeId;
    use pumpkin_command::node::dispatcher::SignableArgument;
    use std::num::NonZeroUsize;

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
}
