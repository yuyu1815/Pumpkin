use pumpkin_data::sound::Sound;
use pumpkin_protocol::java::client::play::SuggestionProviders;
use pumpkin_util::identifier::Identifier;

use crate::{
    argument_types::{ArgumentType, FromStringReader, argument_type::JavaClientArgumentType},
    context::command_context::CommandContext,
    errors::command_syntax_error::CommandSyntaxError,
    source::CommandSource,
    string_reader::StringReader,
    suggestion::suggestions::{Suggestions, SuggestionsBuilder},
};

/// An identifier argument with completions from Pumpkin's supported sound registry.
pub struct SoundArgumentType;

impl<S: CommandSource> ArgumentType<S> for SoundArgumentType {
    type Item = Identifier;

    fn parse(&self, reader: &mut StringReader) -> Result<Self::Item, CommandSyntaxError> {
        Identifier::from_reader(reader)
    }

    fn list_suggestions(
        &self,
        _context: &CommandContext<S>,
        builder: SuggestionsBuilder,
    ) -> Suggestions {
        let remaining = builder.remaining_lowercase().to_owned();
        let mut suggestions = builder;
        for sound in Sound::slice() {
            let name = format!("minecraft:{}", sound.to_name());
            if name.starts_with(&remaining) {
                suggestions = suggestions.suggest(name);
            }
        }
        suggestions.build()
    }

    fn client_side_parser(&self) -> JavaClientArgumentType {
        JavaClientArgumentType::ResourceLocation
    }

    fn override_suggestion_providers(&self) -> Option<SuggestionProviders> {
        Some(SuggestionProviders::AvailableSounds)
    }

    fn examples(&self) -> Vec<String> {
        vec!["minecraft:ambient.cave".to_owned()]
    }
}
