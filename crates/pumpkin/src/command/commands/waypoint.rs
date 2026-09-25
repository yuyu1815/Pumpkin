use pumpkin_data::translation;
use pumpkin_util::PermissionLvl;
use pumpkin_util::permission::{Permission, PermissionDefault, PermissionRegistry};
use pumpkin_util::text::TextComponent;

use crate::command::argument_builder::{ArgumentBuilder, argument, command, literal};
use crate::command::argument_types::entity::EntityArgumentType;
use crate::command::argument_types::hex_color::HexColorArgumentType;
use crate::command::argument_types::identifier::IdentifierArgumentType;
use crate::command::argument_types::team_color::TeamColorArgumentType;
use crate::command::context::command_context::CommandContext;
use crate::command::errors::error_types::CommandErrorType;
use crate::command::node::dispatcher::CommandDispatcher;
use crate::command::node::{CommandExecutor, CommandExecutorResult};

const DESCRIPTION: &str = "List or modify waypoints.";
const PERMISSION: &str = "minecraft:command.waypoint";
const WAYPOINT_MODIFICATION_UNAVAILABLE: CommandErrorType<0> = CommandErrorType::new(
    "commands.waypoint.modify.unavailable",
    "commands.waypoint.modify.unavailable",
);

struct ListExecutor;

impl CommandExecutor for ListExecutor {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let world = context.source.world();
        let dimension = world.dimension.minecraft_name.to_string();

        context.source.send_feedback(
            pumpkin_macros::translate_cross!(
                translation::java::COMMANDS_WAYPOINT_LIST_EMPTY,
                translation::java::COMMANDS_WAYPOINT_LIST_EMPTY,
                TextComponent::text(dimension)
            ),
            false,
        );
        Ok(0)
    }
}

struct ColorExecutor;

impl CommandExecutor for ColorExecutor {
    fn execute(&self, _context: &CommandContext) -> CommandExecutorResult {
        Err(WAYPOINT_MODIFICATION_UNAVAILABLE.create_without_context(TextComponent::text(
            "Waypoint modification is unavailable: current waypoint style, color, and target state are not tracked.",
        )))
    }
}

struct StyleExecutor;

impl CommandExecutor for StyleExecutor {
    fn execute(&self, _context: &CommandContext) -> CommandExecutorResult {
        Err(WAYPOINT_MODIFICATION_UNAVAILABLE.create_without_context(TextComponent::text(
            "Waypoint modification is unavailable: current waypoint style, color, and target state are not tracked.",
        )))
    }
}

pub fn register(dispatcher: &mut CommandDispatcher, registry: &PermissionRegistry) {
    registry.register_permission_or_panic(Permission::new(
        PERMISSION,
        DESCRIPTION,
        PermissionDefault::Op(PermissionLvl::Two),
    ));

    let color_node = literal("color")
        .then(argument("color", TeamColorArgumentType).executes(ColorExecutor))
        .then(literal("hex").then(argument("color", HexColorArgumentType).executes(ColorExecutor)))
        .then(literal("reset").executes(ColorExecutor));

    let style_node = literal("style")
        .then(literal("reset").executes(StyleExecutor))
        .then(
            literal("set").then(argument("style", IdentifierArgumentType).executes(StyleExecutor)),
        );

    let modify_node = literal("modify").then(
        argument("waypoint", EntityArgumentType::Entity)
            .then(color_node)
            .then(style_node),
    );

    dispatcher.register(
        command("waypoint", DESCRIPTION)
            .requires(PERMISSION)
            .then(literal("list").executes(ListExecutor))
            .then(modify_node),
    );
}
