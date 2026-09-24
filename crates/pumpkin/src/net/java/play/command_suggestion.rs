#[allow(clippy::wildcard_imports)]
use super::*;

impl JavaClient {
    pub fn handle_command_suggestion(
        &self,
        player: &Arc<Player>,
        packet: &SCommandSuggestion<'_>,
        server: &Arc<Server>,
    ) {
        let Some(cmd) = &packet.command.get(1..) else {
            return;
        };

        let suggestions = server
            .command_dispatcher
            .load()
            .suggest_with_range(cmd, &player.get_command_source(server));
        let (start, length) = packet_suggestion_range(
            packet.command,
            suggestions.range.start,
            suggestions.range.end,
        );

        let response = CCommandSuggestions::new(
            packet.id,
            (start as i32).into(),
            (length as i32).into(),
            suggestions
                .suggestions
                .into_iter()
                .map(
                    |suggestion| pumpkin_protocol::java::client::play::CommandSuggestion {
                        suggestion: suggestion.text.cached_text().clone(),
                        tooltip: suggestion.tooltip,
                    },
                )
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );

        player.try_send_client_packet(&response);
    }
}

/// Convert a dispatcher range (UTF-8 bytes relative to the slashless command)
/// to the Java client packet's UTF-16 character indices in the full command.
fn packet_suggestion_range(command: &str, range_start: usize, range_end: usize) -> (usize, usize) {
    let start = 1 + command[1..1 + range_start].encode_utf16().count();
    let length = command[1 + range_start..1 + range_end]
        .encode_utf16()
        .count();
    (start, length)
}

#[cfg(test)]
mod tests {
    use super::packet_suggestion_range;

    #[test]
    fn quoted_phrase_with_embedded_space_replaces_the_whole_argument() {
        let command = "/datapack enable \"foo ba";
        let range = 16..command.len() - 1;
        assert_eq!(
            packet_suggestion_range(command, range.start, range.end),
            (17, 7)
        );
    }

    #[test]
    fn completed_quoted_phrase_replaces_the_whole_argument() {
        let command = "/datapack enable \"foo bar\"";
        let range = 16..command.len() - 1;
        assert_eq!(
            packet_suggestion_range(command, range.start, range.end),
            (17, 9)
        );
    }

    #[test]
    fn non_ascii_text_before_range_uses_utf16_and_accounts_for_slash() {
        let command = "/say 😀 \"foo ba";
        let start = command[1..].find('\"').unwrap();
        let range = start..command.len() - 1;
        assert_eq!(
            packet_suggestion_range(command, range.start, range.end),
            (8, 7)
        );
    }
}
