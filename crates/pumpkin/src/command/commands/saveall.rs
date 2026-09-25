use pumpkin_data::translation;
use pumpkin_util::PermissionLvl;
use pumpkin_util::permission::{Permission, PermissionDefault, PermissionRegistry};
use pumpkin_util::text::TextComponent;
use tracing::error;

use crate::command::argument_builder::{ArgumentBuilder, command, literal};
use crate::command::context::command_context::CommandContext;
use crate::command::node::dispatcher::CommandDispatcher;
use crate::command::node::{CommandExecutor, CommandExecutorResult};

const DESCRIPTION: &str = "Saves the server to disk.";

const PERMISSION: &str = "minecraft:command.save-all";

struct SaveAllExecutor {
    flush: bool,
}

impl CommandExecutor for SaveAllExecutor {
    fn execute(&self, context: &CommandContext) -> CommandExecutorResult {
        context.source.send_feedback(
            TextComponent::translate_cross(
                translation::java::COMMANDS_SAVE_SAVING,
                translation::bedrock::COMMANDS_SAVE_START,
                [],
            ),
            false,
        );

        let server_arc = context.server().clone();
        let server_clone = server_arc.clone();
        let source = context.source.clone();
        let flush = self.flush;
        server_arc.spawn_task(async move {
            if let Err(err) = server_clone.save_all_with_flush(flush).await {
                error!("Failed to save server data: {err}");
                source.send_error(TextComponent::translate_cross(
                    translation::java::COMMANDS_SAVE_FAILED,
                    translation::bedrock::COMMANDS_SAVE_FAILED,
                    [],
                ));
            } else {
                source.send_feedback(
                    TextComponent::translate_cross(
                        translation::java::COMMANDS_SAVE_SUCCESS,
                        translation::bedrock::COMMANDS_SAVE_SUCCESS,
                        [],
                    ),
                    true,
                );
            }
        });

        Ok(1)
    }
}

pub fn register(dispatcher: &mut CommandDispatcher, registry: &PermissionRegistry) {
    registry.register_permission_or_panic(Permission::new(
        PERMISSION,
        DESCRIPTION,
        PermissionDefault::Op(PermissionLvl::Four),
    ));

    dispatcher.register(
        command("save-all", DESCRIPTION)
            .requires(PERMISSION)
            .executes(SaveAllExecutor { flush: false })
            .then(literal("flush").executes(SaveAllExecutor { flush: true })),
    );
}

#[cfg(test)]
mod tests {
    use crate::command::CommandSender;
    use pumpkin_config::{AdvancedConfiguration, BasicConfiguration, TelemetryConfig};
    use pumpkin_util::math::vector2::Vector2;
    use pumpkin_world::chunk::io::Dirtiable;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    fn vanilla_data() -> crate::data::VanillaData {
        crate::data::VanillaData {
            banned_ip_list: std::sync::RwLock::new(crate::data::banned_ip::BannedIpList::default()),
            banned_player_list: std::sync::RwLock::new(
                crate::data::banned_player::BannedPlayerList::default(),
            ),
            operator_config: std::sync::RwLock::new(crate::data::op::OperatorConfig::default()),
            user_cache: std::sync::RwLock::new(crate::data::usercache::UserCache::default()),
            whitelist_config: std::sync::RwLock::new(
                crate::data::whitelist::WhitelistConfig::default(),
            ),
        }
    }

    #[tokio::test]
    async fn flush_command_propagates_region_writer_error() {
        let temp_dir = TempDir::new().expect("command flush tempdir");
        let basic = BasicConfiguration {
            default_level_name: temp_dir.path().join("world").display().to_string(),
            allow_nether: false,
            allow_end: false,
            use_favicon: false,
            ..BasicConfiguration::default()
        };

        let server = crate::server::Server::new(
            basic,
            AdvancedConfiguration::default(),
            TelemetryConfig {
                enabled: false,
                ..TelemetryConfig::default()
            },
            vanilla_data(),
        )
        .await;
        let world = server
            .worlds
            .load()
            .first()
            .cloned()
            .expect("overworld loaded");
        let pos = Vector2::new(0, 0);
        world.level.get_or_fetch_chunk(pos, |_| ()).await;
        assert!(
            world
                .level
                .read_chunk_sync(&pos, |chunk| chunk.mark_dirty(true))
                .is_some()
        );

        let blocked_temp = world
            .level
            .level_folder
            .region_folder
            .join("r.0.0.mca")
            .with_extension("tmp");
        tokio::fs::create_dir_all(&blocked_temp)
            .await
            .expect("block Anvil temp path");

        let output = Arc::new(Mutex::new(Vec::<String>::new()));
        let source = CommandSender::Rcon(output.clone()).into_source(&server);
        assert_eq!(
            server
                .command_dispatcher
                .load()
                .execute_input("save-all flush", &source),
            Ok(1)
        );

        let received_error =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if output.lock().unwrap().iter().any(|line| {
                        line.contains("Unable to save") || line.contains("Saving failed")
                    }) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .is_ok();
        let output = output.lock().unwrap().clone();
        assert!(
            received_error
                && output
                    .iter()
                    .any(|line| line.contains("Unable to save") || line.contains("Saving failed")),
            "flush error feedback missing: {output:?}"
        );
        assert!(
            !output.iter().any(|line| line.contains("Saved the game")),
            "failed flush must not report success: {output:?}"
        );

        tokio::fs::remove_dir(&blocked_temp)
            .await
            .expect("remove blocked temp path");
        server.shutdown().await.expect("server shutdown");
        drop(temp_dir);
    }
}
