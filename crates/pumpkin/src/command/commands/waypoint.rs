use pumpkin_data::translation;
use pumpkin_protocol::java::client::play::{
    CWaypoint, TrackedWaypoint, WaypointIcon, WaypointOperation, WaypointTarget,
};
use std::collections::HashMap;

use pumpkin_util::PermissionLvl;
use pumpkin_util::permission::{Permission, PermissionDefault, PermissionRegistry};
use pumpkin_util::text::TextComponent;

use crate::command::argument_builder::{ArgumentBuilder, argument, command, literal};
use crate::command::argument_types::entity::EntityArgumentType;
use crate::command::argument_types::hex_color::HexColorArgumentType;
use crate::command::argument_types::identifier::IdentifierArgumentType;
use crate::command::argument_types::team_color::TeamColorArgumentType;
use crate::command::context::command_context::CommandContext;
use crate::command::node::dispatcher::CommandDispatcher;
use crate::command::node::{CommandExecutor, CommandExecutorResult};
use crate::entity::player::{JavaPlayer, PlayerWaypoint};

const DESCRIPTION: &str = "List or modify waypoints.";
const PERMISSION: &str = "minecraft:command.waypoint";
enum Modification {
    Color(Option<u32>),
    Style(String),
}

#[derive(Clone, Copy)]
enum ColorKind {
    Named,
    Hex,
    Reset,
}
#[derive(Clone, Copy)]
enum StyleKind {
    Set,
    Reset,
}

fn modify_waypoint(
    states: &mut HashMap<uuid::Uuid, PlayerWaypoint>,
    uuid: uuid::Uuid,
    position: pumpkin_util::math::position::BlockPos,
    modification: &Modification,
) -> WaypointOperation {
    let operation = if states.contains_key(&uuid) {
        WaypointOperation::Update
    } else {
        WaypointOperation::Track
    };
    let state = states.entry(uuid).or_insert_with(|| PlayerWaypoint {
        style: "minecraft:default".to_owned(),
        color: None,
        position,
    });
    state.position = position;
    match modification {
        Modification::Color(color) => state.color = *color,
        Modification::Style(style) => state.style.clone_from(style),
    }
    operation
}

struct ListExecutor;

impl CommandExecutor for ListExecutor {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let player = context.source.player_or_err()?;
        let waypoints = player
            .waypoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if waypoints.is_empty() {
            let dimension = context.source.world().dimension.minecraft_name.to_string();
            context.source.send_feedback(
                pumpkin_macros::translate_cross!(
                    translation::java::COMMANDS_WAYPOINT_LIST_EMPTY,
                    translation::java::COMMANDS_WAYPOINT_LIST_EMPTY,
                    TextComponent::text(dimension)
                ),
                false,
            );
        } else {
            let mut entries = waypoints.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(uuid, _)| **uuid);
            for (uuid, waypoint) in entries {
                context.source.send_feedback(
                    TextComponent::text(format!(
                        "{}: {} ({}, {}, {})",
                        uuid,
                        waypoint.style,
                        waypoint.position.0.x,
                        waypoint.position.0.y,
                        waypoint.position.0.z
                    )),
                    false,
                );
            }
        }
        Ok(waypoints.len() as i32)
    }
}

struct ModifyExecutor(Modification);

impl CommandExecutor for ModifyExecutor {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let target = EntityArgumentType::get_entity(context, "waypoint")?;
        let owner = context.source.player_or_err()?;
        let target_entity = target.get_entity();
        let uuid = target_entity.entity_uuid;
        let position = target_entity.block_pos.load();
        let owner_entity = context
            .source
            .entity
            .clone()
            .expect("player source has entity");
        let mut states = owner
            .waypoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let operation = modify_waypoint(&mut states, uuid, position, &self.0);
        let state = states.get(&uuid).expect("modified waypoint exists");
        if let Some(owner) = owner_entity.get_player() {
            let packet = CWaypoint::new(
                operation,
                TrackedWaypoint {
                    identifier: pumpkin_protocol::java::client::play::WaypointIdentifier::Uuid(
                        uuid,
                    ),
                    icon: WaypointIcon {
                        style: &state.style,
                        color: state.color,
                    },
                    target: WaypointTarget::Position(state.position),
                },
            );
            JavaPlayer(owner).try_enqueue_packet(&packet);
        }
        Ok(1)
    }
}

struct ColorExecutor(ColorKind);
impl CommandExecutor for ColorExecutor {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let color = match self.0 {
            ColorKind::Reset => None,
            ColorKind::Hex => {
                let rgb = HexColorArgumentType::get(context, "color")?;
                Some((u32::from(rgb.red) << 16) | (u32::from(rgb.green) << 8) | u32::from(rgb.blue))
            }
            ColorKind::Named => {
                let rgb = TeamColorArgumentType::get(context, "color")?.to_rgb();
                Some((u32::from(rgb.red) << 16) | (u32::from(rgb.green) << 8) | u32::from(rgb.blue))
            }
        };
        ModifyExecutor(Modification::Color(color)).execute(context)
    }
}

struct StyleExecutor(StyleKind);
impl CommandExecutor for StyleExecutor {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        let style = match self.0 {
            StyleKind::Reset => "minecraft:default".to_owned(),
            StyleKind::Set => IdentifierArgumentType::get(context, "style")?.to_string(),
        };
        ModifyExecutor(Modification::Style(style)).execute(context)
    }
}

#[cfg(test)]
mod tests {
    use super::{Modification, modify_waypoint};
    use crate::entity::player::PlayerWaypoint;
    use pumpkin_protocol::java::client::play::WaypointOperation;
    use pumpkin_util::math::position::BlockPos;
    use std::collections::HashMap;
    use uuid::Uuid;

    #[test]
    fn first_modify_tracks_then_updates_preserving_other_state() {
        let uuid = Uuid::new_v4();
        let mut states = HashMap::new();
        assert_eq!(
            modify_waypoint(
                &mut states,
                uuid,
                BlockPos::new(1, 2, 3),
                &Modification::Color(Some(0x112233))
            ),
            WaypointOperation::Track
        );
        assert_eq!(
            modify_waypoint(
                &mut states,
                uuid,
                BlockPos::new(4, 5, 6),
                &Modification::Style("minecraft:custom".to_owned())
            ),
            WaypointOperation::Update
        );
        assert_eq!(
            states[&uuid],
            PlayerWaypoint {
                style: "minecraft:custom".to_owned(),
                color: Some(0x112233),
                position: BlockPos::new(4, 5, 6)
            }
        );
    }
}

pub fn register(dispatcher: &mut CommandDispatcher, registry: &PermissionRegistry) {
    registry.register_permission_or_panic(Permission::new(
        PERMISSION,
        DESCRIPTION,
        PermissionDefault::Op(PermissionLvl::Two),
    ));

    let color_node = literal("color")
        .then(argument("color", TeamColorArgumentType).executes(ColorExecutor(ColorKind::Named)))
        .then(
            literal("hex").then(
                argument("color", HexColorArgumentType).executes(ColorExecutor(ColorKind::Hex)),
            ),
        )
        .then(literal("reset").executes(ColorExecutor(ColorKind::Reset)));

    let style_node = literal("style")
        .then(literal("reset").executes(StyleExecutor(StyleKind::Reset)))
        .then(literal("set").then(
            argument("style", IdentifierArgumentType).executes(StyleExecutor(StyleKind::Set)),
        ));

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
