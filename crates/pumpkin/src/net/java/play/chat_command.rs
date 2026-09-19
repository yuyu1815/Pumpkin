#[allow(clippy::wildcard_imports)]
use super::*;
use pumpkin_command::node::dispatcher::SignableArgument;
use pumpkin_protocol::java::server::play::SChatCommandSigned;
use std::collections::HashSet;

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

    pub async fn handle_signed_chat_command(
        &self,
        player: &Arc<Player>,
        server: &Arc<Server>,
        packet: SignedCommandPacket,
    ) {
        let source = player.get_command_source(server);
        let dispatcher = server.command_dispatcher.load();
        let parsed = dispatcher.parse_input(&packet.command, &source);
        let signable = parsed.signable_arguments();

        // Only a complete parse with no opt-in arguments is an unsigned seam.
        // Decode/parse failure never falls back to the ordinary command path.
        let Some(signable) = signable else {
            warn!(
                player = %player.gameprofile.name,
                "Rejected signed command with malformed or permission-filtered parse"
            );
            return;
        };

        if signable.is_empty() && packet.argument_signatures.is_empty() {
            let command = SChatCommand {
                command: &packet.command,
            };
            self.handle_chat_command(player, server, &command).await;
            return;
        }

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

        // The raw ranges and packet names are retained above, but RSA,
        // session, last-seen, and chain verification are not connected yet.
        warn!(
            player = %player.gameprofile.name,
            "Rejected signable command until its signature chain is verified"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SignedCommandArgument, SignedCommandArgumentError, validate_signed_command_arguments,
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
