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
        let Some((start, length)) = packet_suggestion_range(
            packet.command,
            suggestions.range.start,
            suggestions.range.end,
        ) else {
            return;
        };

        let response = CCommandSuggestions::new(
            packet.id,
            start.into(),
            length.into(),
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
fn packet_suggestion_range(
    command: &str,
    range_start: usize,
    range_end: usize,
) -> Option<(i32, i32)> {
    let command = command.strip_prefix('/')?;
    if range_start > range_end
        || range_end > command.len()
        || !command.is_char_boundary(range_start)
        || !command.is_char_boundary(range_end)
    {
        return None;
    }

    let start = 1usize.checked_add(command[..range_start].encode_utf16().count())?;
    let length = command[range_start..range_end].encode_utf16().count();
    Some((i32::try_from(start).ok()?, i32::try_from(length).ok()?))
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
            Some((17, 7))
        );
    }

    #[test]
    fn completed_quoted_phrase_replaces_the_whole_argument() {
        let command = "/datapack enable \"foo bar\"";
        let range = 16..command.len() - 1;
        assert_eq!(
            packet_suggestion_range(command, range.start, range.end),
            Some((17, 9))
        );
    }

    #[test]
    fn non_ascii_text_before_range_uses_utf16_and_accounts_for_slash() {
        let command = "/say 😀 \"foo ba";
        let start = command[1..].find('\"').unwrap();
        let range = start..command.len() - 1;
        assert_eq!(
            packet_suggestion_range(command, range.start, range.end),
            Some((8, 7))
        );
    }

    #[test]
    fn reversed_range_is_invalid() {
        assert_eq!(packet_suggestion_range("/hello", 4, 2), None);
    }

    #[test]
    fn out_of_bounds_range_is_invalid() {
        assert_eq!(packet_suggestion_range("/hello", 0, 6), None);
    }

    #[test]
    fn range_inside_multibyte_character_is_invalid() {
        assert_eq!(packet_suggestion_range("/é", 1, 2), None);
    }

    #[test]
    fn range_inside_emoji_is_invalid() {
        assert_eq!(packet_suggestion_range("/😀", 1, 4), None);
    }
}
