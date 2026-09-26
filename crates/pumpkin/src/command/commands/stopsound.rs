use pumpkin_data::translation;
use pumpkin_util::PermissionLvl;
use pumpkin_util::permission::{Permission, PermissionDefault, PermissionRegistry};
use pumpkin_util::text::TextComponent;

use crate::command::argument_builder::{ArgumentBuilder, argument, command, literal};
use crate::command::argument_types::entity::EntityArgumentType;
use crate::command::argument_types::identifier::IdentifierArgumentType;
use crate::command::argument_types::sound::SoundArgumentType;
use crate::command::argument_types::sound_category::SoundCategoryArgumentType;
use crate::command::context::command_context::CommandContext;
use crate::command::node::dispatcher::CommandDispatcher;
use crate::command::node::{CommandExecutor, CommandExecutorResult};

const DESCRIPTION: &str = "Stops a currently playing sound.";
const PERMISSION: &str = "minecraft:command.stopsound";

enum StopSoundMode {
    All,
    Category,
    Sound,
    CategoryAndSound,
}

struct StopSoundExecutor(StopSoundMode);

impl CommandExecutor for StopSoundExecutor {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let targets = EntityArgumentType::get_players(context, "targets")?;

        let (category, sound) = match self.0 {
            StopSoundMode::All => (None, None),
            StopSoundMode::Category => {
                let cat = SoundCategoryArgumentType::get(context, "source")?;
                (Some(cat), None)
            }
            StopSoundMode::Sound => {
                let snd = IdentifierArgumentType::get(context, "sound")?;
                (None, Some(snd.to_string()))
            }
            StopSoundMode::CategoryAndSound => {
                let cat = SoundCategoryArgumentType::get(context, "source")?;
                let snd = IdentifierArgumentType::get(context, "sound")?;
                (Some(cat), Some(snd.to_string()))
            }
        };

        for target in &targets {
            target.stop_sound(sound.clone(), category);
        }

        let text = match (category, &sound) {
            (Some(c), Some(s)) => TextComponent::translate_cross(
                translation::java::COMMANDS_STOPSOUND_SUCCESS_SOURCE_SOUND,
                translation::java::COMMANDS_STOPSOUND_SUCCESS_SOURCE_SOUND,
                [
                    TextComponent::text(s.clone()),
                    TextComponent::text(c.to_name()),
                ],
            ),
            (Some(c), None) => TextComponent::translate_cross(
                translation::java::COMMANDS_STOPSOUND_SUCCESS_SOURCE_ANY,
                translation::java::COMMANDS_STOPSOUND_SUCCESS_SOURCE_ANY,
                [TextComponent::text(c.to_name())],
            ),
            (None, Some(s)) => TextComponent::translate_cross(
                translation::java::COMMANDS_STOPSOUND_SUCCESS_SOURCELESS_SOUND,
                translation::java::COMMANDS_STOPSOUND_SUCCESS_SOURCELESS_SOUND,
                [TextComponent::text(s.clone())],
            ),
            (None, None) => TextComponent::translate_cross(
                translation::java::COMMANDS_STOPSOUND_SUCCESS_SOURCELESS_ANY,
                translation::java::COMMANDS_STOPSOUND_SUCCESS_SOURCELESS_ANY,
                [],
            ),
        };
        context.source.send_feedback(text, true);

        Ok(targets.len() as i32)
    }
}

pub fn register(dispatcher: &mut CommandDispatcher, registry: &PermissionRegistry) {
    registry.register_permission_or_panic(Permission::new(
        PERMISSION,
        DESCRIPTION,
        PermissionDefault::Op(PermissionLvl::Two),
    ));

    dispatcher.register(
        command("stopsound", DESCRIPTION).requires(PERMISSION).then(
            argument("targets", EntityArgumentType::Players)
                .executes(StopSoundExecutor(StopSoundMode::All))
                .then(
                    literal("*").then(
                        argument("sound", SoundArgumentType)
                            .executes(StopSoundExecutor(StopSoundMode::Sound)),
                    ),
                )
                .then(
                    argument("source", SoundCategoryArgumentType)
                        .executes(StopSoundExecutor(StopSoundMode::Category))
                        .then(
                            argument("sound", SoundArgumentType)
                                .executes(StopSoundExecutor(StopSoundMode::CategoryAndSound)),
                        ),
                ),
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::commands::playsound;
    use crate::command::context::command_source::CommandSource;
    use crate::command::node::dispatcher::CommandDispatcher;
    use std::sync::Arc;

    #[test]
    fn sound_arguments_suggest_supported_sounds() {
        let mut dispatcher = CommandDispatcher::new();
        let registry = PermissionRegistry::default();
        playsound::register(&mut dispatcher, &registry);
        register(&mut dispatcher, &registry);
        let source = Arc::new(CommandSource::dummy());

        for input in [
            "playsound minecraft:entity.player.",
            "stopsound @a * minecraft:entity.player.",
            "stopsound @a master minecraft:entity.player.",
        ] {
            let suggestions = dispatcher.suggest_with_range(input, &source);
            assert!(
                suggestions
                    .suggestions
                    .iter()
                    .any(|suggestion| suggestion.text_as_string()
                        == "minecraft:entity.player.levelup"),
                "missing sound completion for {input}"
            );
        }
    }
}
