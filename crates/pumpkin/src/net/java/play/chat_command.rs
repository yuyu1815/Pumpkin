#[allow(clippy::wildcard_imports)]
use super::*;
use crate::command::{CommandSource, dispatcher::CommandDispatcher};
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
    Duplicate(String),
    Missing(String),
}

fn signed_command_event_matches(expected: &str, actual: &str) -> bool {
    expected == actual
}

fn validate_signed_command_arguments(
    parsed: Option<&[SignableArgument]>,
    signatures: &[SignedCommandArgument],
) -> Result<(), SignedCommandArgumentError> {
    let Some(parsed) = parsed else {
        return Err(SignedCommandArgumentError::Malformed);
    };

    let mut expected = HashSet::with_capacity(parsed.len());
    for argument in parsed {
        if !expected.insert(argument.name.as_str()) {
            return Err(SignedCommandArgumentError::Duplicate(argument.name.clone()));
        }
    }

    let mut received = HashSet::with_capacity(signatures.len());
    for signature in signatures {
        if !received.insert(signature.name.as_str()) {
            return Err(SignedCommandArgumentError::Duplicate(
                signature.name.clone(),
            ));
        }
        if !expected.contains(signature.name.as_str()) {
            return Err(SignedCommandArgumentError::Unknown(signature.name.clone()));
        }
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

    fn execute_authenticated_chat_command<'a>(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        command: &str,
        dispatcher: &'a CommandDispatcher,
        parsed: ParsingResult<'a, CommandSource>,
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
        if let Err(error) = dispatcher.execute(parsed) {
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
        let source = player.get_command_source(server);
        let dispatcher = server.command_dispatcher.load();
        let parsed = dispatcher.parse_input(&packet.command, &source);
        let signable = parsed.signable_arguments();

        // A signed packet must parse completely; decode/parse failure never
        // falls back to the ordinary unsigned command path.
        let Some(signable) = signable else {
            warn!(
                player = %player.gameprofile.name,
                "Rejected signed command with malformed or permission-filtered parse"
            );
            return;
        };

        if let Err(error) =
            validate_signed_command_arguments(Some(&signable), &packet.argument_signatures)
        {
            crate::net::chat::state::break_inbound_chain(player);
            warn!(
                player = %player.gameprofile.name,
                ?error,
                "Rejected signed command argument set"
            );
            return;
        }

        let mut contents = Vec::with_capacity(packet.argument_signatures.len());
        let mut signatures = Vec::with_capacity(packet.argument_signatures.len());
        for entry in &packet.argument_signatures {
            let Some(argument) = signable.iter().find(|argument| argument.name == entry.name)
            else {
                crate::net::chat::state::break_inbound_chain(player);
                return;
            };
            contents.push(argument.raw_value(&packet.command));
            signatures.push(entry.signature.as_slice());
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let (session_id, public_key) = {
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
                return;
            }
            (chat_session.session_id, chat_session.public_key.clone())
        };

        if let Err(error) = crate::net::chat::state::verify_signed_command_and_commit(
            player,
            session_id,
            &public_key,
            packet.timestamp,
            packet.salt,
            packet.message_count,
            &packet.acknowledged,
            packet.checksum,
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

        // Execute the exact parse result that was authenticated. The event may
        // observe the command, but cannot replace it with an unauthenticated one.
        self.execute_authenticated_chat_command(
            player,
            server,
            &packet.command,
            &dispatcher,
            parsed,
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
            Err(SignedCommandArgumentError::Duplicate("message".into()))
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
