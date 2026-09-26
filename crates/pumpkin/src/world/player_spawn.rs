use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering},
};

use bytes::BufMut;
use tracing::{debug, info};
use uuid::Uuid;

use crate::{
    command::client_suggestions,
    entity::{EntityBase, player::Player},
    net::{ClientPlatform, java::JavaClient},
    plugin::player::player_join::PlayerJoinEvent,
    server::Server,
    world::{World, chunker},
};
use pumpkin_config::BasicConfiguration;
use pumpkin_data::{
    data_component_impl::EquipmentSlot,
    entity::EntityType,
    item::{BedrockItem, BedrockItemVersion},
    translation,
};
use pumpkin_inventory::crafting::recipe_provider::RecipeProvider;
use pumpkin_inventory::screen_handler::InventoryPlayer;
use pumpkin_protocol::bedrock::client::set_actor_data::{CSetActorData, PropertySyncData};
use pumpkin_protocol::codec::item_stack_seralizer::ItemStackSerializer;
use pumpkin_protocol::java;
use pumpkin_protocol::{
    bedrock::{
        client::{
            CBiomeDefinitionList, CJigsawStructureData, CVoxelShapes,
            add_player::CAddPlayer,
            common::BuildPlatform,
            creative_content::{
                CCreativeContent, CreativeCategory, CreativeGroupInfoPayload,
                CreativeItemEntryPayload,
            },
            item_registry::{CItemRegistry, ItemData},
            player_list::{CPlayerList, PlayerListEntry},
            start_game::{
                CStartGame, Experiments, GamePublishSetting, LevelSettings, ServerTelemetryData,
            },
            update_attributes::{AttributeData, CUpdateAttributes},
        },
        network_item::{NetworkItemDescriptor, NetworkItemStackDescriptor},
    },
    codec::{var_int::VarInt, var_long::VarLong, var_uint::VarUInt, var_ulong::VarULong},
    java::client::play::{
        CGameEvent, CLogin, CPlayerInfoUpdate, CPlayerSpawnPosition, CRecipeBookAdd,
        CRecipeBookSettings, CSetEntityMetadata, CSetEquipment, CSetSelectedSlot, CSpawnEntity,
        CUpdateRecipes, GameEvent, InitChat, Metadata, PlayerAction, PlayerInfoFlags,
        PlayerSpawnData,
    },
};
use pumpkin_util::{
    math::{position::BlockPos, vector2::Vector2, vector3::Vector3},
    resource_location::ResourceLocation,
    text::{TextComponent, color::NamedColor},
    version::JavaMinecraftVersion,
};
use pumpkin_world::{CURRENT_BEDROCK_MC_VERSION, biome};

impl World {
    #[allow(clippy::too_many_lines)]
    pub async fn spawn_bedrock_player(
        &self,
        base_config: &BasicConfiguration,
        player: Arc<Player>,
        server: &Arc<Server>,
    ) {
        static CREATIVE_CONTENT: std::sync::OnceLock<(
            Vec<CreativeGroupInfoPayload>,
            Vec<CreativeItemEntryPayload>,
        )> = std::sync::OnceLock::new();

        static BEDROCK_CRAFTING_DATA: std::sync::OnceLock<
            Vec<pumpkin_protocol::bedrock::client::BedrockRecipe>,
        > = std::sync::OnceLock::new();

        let level_info = server.level_info.load();
        let (rain_level, lightning_level) = {
            let weather = self
                .weather
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (weather.rain_level, weather.thunder_level)
        };
        let runtime_id = player.entity_id() as u64;
        let (position, yaw, pitch) = if player.has_played_before.load(Ordering::Relaxed) {
            let position = player.position();
            let yaw = player.get_entity().yaw.load(); //info.spawn_angle;
            let pitch = player.get_entity().pitch.load();

            (position, yaw, pitch)
        } else {
            let spawn_position = Vector2::new(level_info.spawn_x, level_info.spawn_z);
            let chunk_pos = Vector2::new(level_info.spawn_x >> 4, level_info.spawn_z >> 4);
            self.level.get_or_fetch_chunk(chunk_pos, |_| ()).await;
            let top = self.get_top_block(spawn_position);
            let pos_y = if top > self.dimension.min_y {
                top + 1
            } else {
                level_info.spawn_y
            };

            let position = Vector3::new(
                f64::from(level_info.spawn_x) + 0.5,
                f64::from(pos_y),
                f64::from(level_info.spawn_z) + 0.5,
            );
            (position, level_info.spawn_yaw, level_info.spawn_pitch)
        };

        // Keep the server-side transform aligned with the StartGame position. In
        // particular, this ensures an early disconnect persists the real spawn.
        player.living_entity.entity.set_pos(position);
        player.living_entity.entity.set_rotation(yaw, pitch);
        player.living_entity.entity.last_pos.store(position);

        // Todo make the data less spread
        let level_settings = LevelSettings {
            seed: self.level.seed.0,
            spawn_biome_type: 0,
            custom_biome_name: String::new(),
            dimension: VarInt(0),
            generator_type: VarInt(1),
            world_gamemode: server
                .defaultgamemode
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .gamemode
                .into(),
            hardcore: base_config.hardcore,
            difficulty: VarInt(level_info.difficulty as i32),
            spawn_position: BlockPos::new(
                level_info.spawn_x,
                level_info.spawn_y,
                level_info.spawn_z,
            ),
            has_achievements_disabled: false,
            editor_world_type: VarInt(0),
            is_created_in_editor: false,
            is_exported_from_editor: false,
            day_cycle_stop_time: VarInt(-1),
            education_edition_offer: VarUInt(0),
            has_education_features_enabled: false,
            education_product_id: String::new(),
            rain_level,
            lightning_level,
            has_confirmed_platform_locked_content: false,
            was_multiplayer_intended: true,
            was_lan_broadcasting_intended: true,
            xbox_live_broadcast_setting: GamePublishSetting::Public,
            platform_broadcast_setting: GamePublishSetting::Public,
            commands_enabled: level_info.allow_commands,
            is_texture_packs_required: false,
            rule_data: Vec::new(),
            experiments: Experiments {
                toggles: Vec::new(),
                experiments_ever_toggled: false,
            },
            bonus_chest: false,
            has_start_with_map_enabled: false,
            // TODO Bedrock permission level are different
            permission_level: 2,
            server_simulation_distance: server
                .advanced_config
                .networking
                .bedrock
                .simulation_distance
                .get()
                .into(),
            has_locked_behavior_pack: false,
            has_locked_resource_pack: false,
            is_from_locked_world_template: false,
            is_using_msa_gamertags_only: false,
            is_from_world_template: false,
            is_world_template_option_locked: false,
            is_only_spawning_v1_villagers: false,
            is_disabling_personas: false,
            is_disabling_custom_skins: false,
            emote_chat_muted: false,
            game_version: CURRENT_BEDROCK_MC_VERSION.into(),
            limited_world_width: 0,
            limited_world_height: 0,
            new_nether: true,
            edu_shared_uri_button_name: String::new(),
            edu_shared_uri_link_uri: String::new(),
            override_force_experimental_gameplay_has_value: false,
            chat_restriction_level: 0,
            disable_player_interactions: false,
            server_editor_connection_policy: VarInt(0),
            allow_anonymous_block_drops_in_editor_worlds: false,
        };
        drop(level_info);

        let Some(client) = player.client.bedrock() else {
            return;
        };

        let start_game = CStartGame {
            entity_id: VarLong(runtime_id as _),
            runtime_entity_id: VarULong(runtime_id),
            player_gamemode: player.gamemode.load().into(),
            // Bedrock represents the local player at eye height; Pumpkin stores feet position.
            position: Vector3::new(
                position.x as f32,
                position.y as f32 + player.get_entity().entity_type.eye_height,
                position.z as f32,
            ),
            pitch,
            yaw,
            level_settings,
            level_id: String::new(),
            level_name: "Pumpkin world".to_string(),
            premium_world_template_id: String::new(),
            is_trial: false,
            rewind_history_size: VarInt(0),
            server_authoritative_block_breaking: true,
            current_level_time: self.get_world_age() as _,
            enchantment_seed: VarInt(0),
            block_properties_size: VarUInt(0),
            // TODO Make this unique
            multiplayer_correlation_id: Uuid::default().to_string(),
            enable_itemstack_net_manager: true,
            server_version: "Pumpkin Rust Server".to_string(),
            compound_id: 10,
            compound_len: VarUInt(0),
            compound_end: 0,
            block_registry_checksum: 0,
            world_template_id: Uuid::nil(),
            enable_clientside_generation: false,
            blocknetwork_ids_are_hashed: true,
            server_auth_sounds: true,
            server_join_information: None,
            telemetry: ServerTelemetryData {
                server_id: String::new(),
                scenario_id: String::new(),
                world_id: String::new(),
                owner_id: String::new(),
            },
        };
        client.send_packet(&CJigsawStructureData).await;
        client.send_packet(&CVoxelShapes).await;
        if let Ok(data) = client.serialize_packet(&start_game) {
            client.send_game_packet(data).await;
        }

        if let Ok(data) = client.serialize_packet(&CBiomeDefinitionList) {
            client.send_game_packet(data).await;
        }

        let item_registry = CItemRegistry {
            items: BedrockItem::ALL_BEDROCK_ITEMS
                .iter()
                .map(|b| ItemData {
                    item_name: b.registry_key.into(),
                    item_id: b.id,
                    is_component_based: b.component_based,
                    item_version: VarInt::from(match b.version {
                        BedrockItemVersion::Legacy => 0,
                        BedrockItemVersion::DataDriven => 1,
                        BedrockItemVersion::None => 2,
                    }),
                    component_data: b.definition_components.into(),
                })
                .collect::<Vec<_>>(),
        };
        if let Ok(data) = client.serialize_packet(&item_registry) {
            client.send_game_packet(data).await;
        }

        let (groups, entries) = CREATIVE_CONTENT.get_or_init(|| {
            let groups = pumpkin_data::bedrock_creative::CREATIVE_GROUPS
                .iter()
                .map(|g| {
                    let creative_category = match g.category {
                        1 => CreativeCategory::Construction,
                        2 => CreativeCategory::Nature,
                        3 => CreativeCategory::Equipment,
                        4 => CreativeCategory::Items,
                        5 => CreativeCategory::ItemCommandOnly,
                        _ => CreativeCategory::Undefined,
                    };
                    let icon_item = if g.icon_item_id != 0 {
                        NetworkItemDescriptor {
                            id: VarInt::from(g.icon_item_id),
                            stack_size: 1,
                            aux_value: VarUInt(g.icon_item_aux_value),
                            block_runtime_id: VarInt(0),
                            nbt_data: pumpkin_nbt::Nbt::default(),
                            place_on_blocks: Vec::new(),
                            destroy_blocks: Vec::new(),
                            shield_blocking_tick: 0,
                        }
                    } else {
                        NetworkItemDescriptor::default()
                    };

                    CreativeGroupInfoPayload {
                        creative_category,
                        name: g.name.to_string(),
                        group_icon_item: icon_item,
                    }
                })
                .collect::<Vec<_>>();

            let entries = pumpkin_data::bedrock_creative::CREATIVE_ENTRIES
                .iter()
                .enumerate()
                .map(|(i, e)| CreativeItemEntryPayload {
                    id: VarUInt((i + 1) as u32),
                    item: NetworkItemDescriptor {
                        id: VarInt::from(e.item_id),
                        stack_size: 1,
                        aux_value: VarUInt(e.item_aux_value),
                        block_runtime_id: VarInt(0),
                        nbt_data: pumpkin_nbt::Nbt::default(),
                        place_on_blocks: Vec::new(),
                        destroy_blocks: Vec::new(),
                        shield_blocking_tick: 0,
                    },
                    group_index: VarUInt(e.group_index),
                })
                .collect::<Vec<_>>();

            (groups, entries)
        });
        let creative_content = CCreativeContent { groups, entries };
        if let Ok(data) = client.serialize_packet(&creative_content) {
            client.send_game_packet(data).await;
        }

        let bedrock_recipes = BEDROCK_CRAFTING_DATA.get_or_init(|| {
            use pumpkin_data::item::{Item, JavaToBedrockItemMapping};
            use pumpkin_data::recipes::{CraftingRecipeTypes, RecipeIngredientTypes};
            use pumpkin_protocol::bedrock::client::{
                BedrockRecipe, BedrockShapedRecipe, BedrockShapelessRecipe, ItemDescriptorCount,
                RecipeUnlockRequirement,
            };
            use pumpkin_protocol::bedrock::network_item::NetworkItemDescriptor;
            use pumpkin_protocol::codec::{var_int::VarInt, var_uint::VarUInt};

            let mut mapped_recipes = Vec::new();
            let mut network_id_counter = 1u32;

            for recipe in pumpkin_data::recipes::RECIPES_CRAFTING {
                let map_ingredient = |ing: &RecipeIngredientTypes| -> ItemDescriptorCount {
                    let item_key = match ing {
                        RecipeIngredientTypes::Simple(name) => Some(*name),
                        RecipeIngredientTypes::Tagged(tag) => {
                            let tag_name = tag.strip_prefix('#').unwrap_or(tag);
                            pumpkin_data::tag::get_tag_ids(
                                pumpkin_data::tag::RegistryKey::Item,
                                tag_name,
                            )
                            .and_then(|ids| {
                                ids.first().and_then(|&first_id| {
                                    Item::from_id(first_id).map(|item| item.registry_key)
                                })
                            })
                        }
                        RecipeIngredientTypes::OneOf(names) => names.first().copied(),
                    };

                    if let Some(key) = item_key {
                        let registry_key = key.strip_prefix("minecraft:").unwrap_or(key);
                        if let Some(item) = Item::from_registry_key(registry_key)
                            && let Some(mapping) =
                                JavaToBedrockItemMapping::from_java_item_id(item.id)
                        {
                            return ItemDescriptorCount {
                                item_identifier: mapping.bedrock_item.registry_key.to_string(),
                                metadata_value: mapping.bedrock_data as i32,
                                count: 1,
                            };
                        }
                    }

                    ItemDescriptorCount {
                        item_identifier: String::new(),
                        metadata_value: 0,
                        count: 0,
                    }
                };

                match recipe {
                    CraftingRecipeTypes::CraftingShaped {
                        category: _,
                        group: _,
                        show_notification: _,
                        key,
                        pattern,
                        result,
                    } => {
                        let height = pattern.len() as i32;
                        let width = pattern.iter().map(|s| s.len()).max().unwrap_or(0) as i32;

                        let mut input = Vec::new();
                        for r in 0..height {
                            let pattern_row = pattern[r as usize];
                            for c in 0..width {
                                let ch = pattern_row.chars().nth(c as usize).unwrap_or(' ');
                                if ch == ' ' {
                                    input.push(ItemDescriptorCount {
                                        item_identifier: String::new(),
                                        metadata_value: 0,
                                        count: 0,
                                    });
                                } else {
                                    let mut ingredient = None;
                                    for &(key_ch, ref ing) in *key {
                                        if key_ch == ch {
                                            ingredient = Some(ing);
                                            break;
                                        }
                                    }
                                    if let Some(ing) = ingredient {
                                        input.push(map_ingredient(ing));
                                    } else {
                                        input.push(ItemDescriptorCount {
                                            item_identifier: String::new(),
                                            metadata_value: 0,
                                            count: 0,
                                        });
                                    }
                                }
                            }
                        }

                        let output_item = Item::from_registry_key(result.id);
                        if let Some(item) = output_item
                            && let Some(mapping) =
                                JavaToBedrockItemMapping::from_java_item_id(item.id)
                        {
                            let output_descriptor = NetworkItemDescriptor {
                                id: VarInt::from(mapping.bedrock_item.id),
                                stack_size: result.count as u16,
                                aux_value: VarUInt(mapping.bedrock_data),
                                block_runtime_id: VarInt::from(mapping.bedrock_block_state),
                                nbt_data: pumpkin_nbt::Nbt::default(),
                                place_on_blocks: Vec::new(),
                                destroy_blocks: Vec::new(),
                                shield_blocking_tick: 0,
                            };

                            mapped_recipes.push(BedrockRecipe::Shaped(BedrockShapedRecipe {
                                recipe_id: format!("pumpkin:recipe_{network_id_counter}"),
                                width: VarInt(width),
                                height: VarInt(height),
                                input,
                                output: vec![output_descriptor],
                                uuid: Uuid::nil(),
                                block: "crafting_table".to_string(),
                                priority: VarInt(1),
                                assume_symmetry: true,
                                unlock_requirement: RecipeUnlockRequirement { context: 1 },
                                recipe_network_id: VarUInt(network_id_counter),
                            }));
                            network_id_counter += 1;
                        }
                    }
                    CraftingRecipeTypes::CraftingShapeless {
                        category: _,
                        group: _,
                        ingredients,
                        result,
                    } => {
                        let input = ingredients.iter().map(map_ingredient).collect::<Vec<_>>();

                        let output_item = Item::from_registry_key(result.id);
                        if let Some(item) = output_item
                            && let Some(mapping) =
                                JavaToBedrockItemMapping::from_java_item_id(item.id)
                        {
                            let output_descriptor = NetworkItemDescriptor {
                                id: VarInt::from(mapping.bedrock_item.id),
                                stack_size: result.count as u16,
                                aux_value: VarUInt(mapping.bedrock_data),
                                block_runtime_id: VarInt::from(mapping.bedrock_block_state),
                                nbt_data: pumpkin_nbt::Nbt::default(),
                                place_on_blocks: Vec::new(),
                                destroy_blocks: Vec::new(),
                                shield_blocking_tick: 0,
                            };

                            mapped_recipes.push(BedrockRecipe::Shapeless(BedrockShapelessRecipe {
                                recipe_id: format!("pumpkin:recipe_{network_id_counter}"),
                                input,
                                output: vec![output_descriptor],
                                uuid: Uuid::nil(),
                                block: "crafting_table".to_string(),
                                priority: VarInt(1),
                                unlock_requirement: RecipeUnlockRequirement { context: 1 },
                                recipe_network_id: VarUInt(network_id_counter),
                            }));
                            network_id_counter += 1;
                        }
                    }
                    _ => {}
                }
            }
            mapped_recipes
        });

        let crafting_data = pumpkin_protocol::bedrock::client::CCraftingData {
            recipes: bedrock_recipes.clone(),
            clean_recipes: false,
        };
        if let Ok(data) = client.serialize_packet(&crafting_data) {
            client.send_game_packet(data).await;
        }

        player.on_screen_handler_opened(&player.player_screen_handler);

        {
            let mut abilities = player
                .abilities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            abilities.set_for_gamemode(player.gamemode.load());
        };

        let entity = &player.get_entity();
        let metadata = entity.bedrock_metadata();

        let actor_data = CSetActorData {
            target_runtime_id: VarULong(runtime_id),
            actor_data: metadata,
            synced_properties: PropertySyncData {
                int_entries_list: HashMap::new(),
                float_entries_list: HashMap::new(),
            },
            tick: VarULong(0),
        };
        if let Ok(data) = client.serialize_packet(&actor_data) {
            client.send_game_packet(data).await;
        }
        player.send_abilities_update();

        {
            let command_dispatcher = server.command_dispatcher.load();
            client_suggestions::send_bedrock_commands_packet(&player, server, &command_dispatcher);
        };

        client
            .enqueue_client_packet(&CUpdateAttributes {
                target_runtime_id: VarULong(runtime_id),
                attribute_list: vec![
                    AttributeData {
                        min_value: 0.0,
                        max_value: 3.402_823_5E38,
                        current_value: 0.1,
                        default_min_value: 0.0,
                        default_max_value: 3.402_823_5E38,
                        default_value: 0.1,
                        name: "minecraft:movement".to_string(),
                        modifiers: Vec::new(),
                    },
                    AttributeData {
                        min_value: 0.0,
                        max_value: 3.402_823_5E38,
                        current_value: 0.02,
                        default_min_value: 0.0,
                        default_max_value: 3.402_823_5E38,
                        default_value: 0.02,
                        name: "minecraft:underwater_movement".to_string(),
                        modifiers: Vec::new(),
                    },
                    AttributeData {
                        min_value: 0.0,
                        max_value: 1.0,
                        current_value: 0.08,
                        default_min_value: 0.0,
                        default_max_value: 1.0,
                        default_value: 0.08,
                        name: "minecraft:gravity".to_string(),
                        modifiers: Vec::new(),
                    },
                    AttributeData {
                        min_value: 0.0,
                        max_value: 400.0,
                        current_value: 400.0,
                        default_min_value: 0.0,
                        default_max_value: 400.0,
                        default_value: 400.0,
                        name: "minecraft:air".to_string(),
                        modifiers: Vec::new(),
                    },
                    AttributeData {
                        min_value: 0.0,
                        max_value: 20.0,
                        current_value: player.living_entity.health.load(),
                        default_min_value: 0.0,
                        default_max_value: 20.0,
                        default_value: 20.0,
                        name: "minecraft:health".to_string(),
                        modifiers: Vec::new(),
                    },
                    AttributeData {
                        min_value: 0.0,
                        max_value: 20.0,
                        current_value: player.hunger_manager.level.load().into(),
                        default_min_value: 0.0,
                        default_max_value: 20.0,
                        default_value: 20.0,
                        name: "minecraft:player.hunger".to_string(),
                        modifiers: Vec::new(),
                    },
                ],
                tick: VarULong(0),
            })
            .await;

        // --- MULTIPLAYER BROADCASTING ---

        let gameprofile = &player.gameprofile;
        let velocity = player.get_entity().velocity.load();

        // 1. Broadcast the new Bedrock player to everyone else (Java + Bedrock)
        let bedrock_player_list = CPlayerList {
            action: CPlayerList::ACTION_ADD,
            entries: vec![PlayerListEntry {
                uuid: gameprofile.id,
                entity_unique_id: VarLong(runtime_id as i64),
                username: gameprofile.name.clone(),
                xuid: String::new(),
                platform_chat_id: String::new(),
                build_platform: BuildPlatform::Unknown,
                skin: (**player.bedrock_skin.load()).clone(),
                is_teacher: false,
                is_host: false,
                is_sub_client: false,
                player_color: [0, 0, 0, 0],
            }],
        };

        let gamemode = player.gamemode.load();
        self.broadcast_packet_except_editioned(
            &[gameprofile.id],
            &CPlayerInfoUpdate::new(
                (PlayerInfoFlags::ADD_PLAYER
                    | PlayerInfoFlags::UPDATE_GAME_MODE
                    | PlayerInfoFlags::UPDATE_LISTED
                    | PlayerInfoFlags::UPDATE_LATENCY
                    | PlayerInfoFlags::UPDATE_LIST_PRIORITY
                    | PlayerInfoFlags::UPDATE_HAT)
                    .bits(),
                &[pumpkin_protocol::java::client::play::Player {
                    uuid: gameprofile.id,
                    actions: &[
                        PlayerAction::AddPlayer {
                            name: &gameprofile.name,
                            properties: &gameprofile.properties.load(),
                        },
                        PlayerAction::UpdateGameMode(VarInt(gamemode as i32)),
                        PlayerAction::UpdateListed(true),
                        PlayerAction::UpdateLatency(VarInt(0)),
                        PlayerAction::UpdateListOrder(VarInt(0)),
                        PlayerAction::UpdateHat(true),
                    ],
                }],
            ),
            &bedrock_player_list,
        );

        let bedrock_add_player = CAddPlayer {
            uuid: gameprofile.id,
            player_name: gameprofile.name.clone(),
            target_runtime_id: VarULong(runtime_id),
            platform_chat_id: String::new(),
            position: Vector3::new(position.x as f32, position.y as f32, position.z as f32),
            velocity: Vector3::new(velocity.x as f32, velocity.y as f32, velocity.z as f32),
            rotation: Vector2::new(pitch, yaw),
            y_head_rotation: yaw,
            carried_item: NetworkItemStackDescriptor::default(),
            player_game_type: player.gamemode.load().into(),
            entity_data: entity.bedrock_metadata(),
            synced_properties: PropertySyncData::default(),
            abilities_data: pumpkin_protocol::bedrock::client::SerializedAbilitiesData {
                target_player_raw_id: runtime_id as i64,
                player_permissions:
                    pumpkin_protocol::bedrock::client::PlayerPermissionLevel::Visitor,
                command_permissions: pumpkin_protocol::bedrock::client::CommandPermissionLevel::Any,
                layers: vec![
                    pumpkin_protocol::bedrock::client::SerializedAbilitiesDataSerializedLayer {
                        serialized_layer: 0,
                        abilities_set: 0,
                        ability_value: 0,
                        fly_speed: 0.05,
                        vertical_fly_speed: 0.05,
                        walk_speed: 0.1,
                    },
                ],
            },
            actor_links: Vec::new(),
            device_id: String::new(),
            build_platform: BuildPlatform::Unknown,
        };

        self.broadcast_packet_except_editioned(
            &[gameprofile.id],
            &CSpawnEntity::new(
                (runtime_id as i32).into(),
                gameprofile.id,
                i32::from(EntityType::PLAYER.id).into(),
                position,
                pitch,
                yaw,
                yaw,
                0.into(),
                velocity,
            ),
            &bedrock_add_player,
        );

        self.send_player_equipment(&player);
        player
            .living_entity
            .send_current_equipment_attribute_modifiers();

        // Broadcast metadata to Java players so they can correctly interact with the new player
        let skin_parts = player.config.load().skin_parts;

        self.broadcast_skin_parts(
            &[gameprofile.id],
            runtime_id as i32,
            skin_parts,
            &actor_data,
        );

        // 2. Spawn existing players for our new Bedrock client
        let players = self.players.load();

        for existing_player in players
            .iter()
            .filter(|p| p.gameprofile.id != gameprofile.id)
        {
            let ex_profile = &existing_player.gameprofile;
            let ex_entity = &existing_player.get_entity();
            let ex_pos = ex_entity.pos.load();
            let ex_vel = ex_entity.velocity.load();

            let ex_player_list = CPlayerList {
                action: CPlayerList::ACTION_ADD,
                entries: vec![PlayerListEntry {
                    uuid: ex_profile.id,
                    entity_unique_id: VarLong(existing_player.entity_id() as i64),
                    username: ex_profile.name.clone(),
                    xuid: String::new(),
                    platform_chat_id: String::new(),
                    build_platform: BuildPlatform::Unknown,
                    skin: (**existing_player.bedrock_skin.load()).clone(),
                    is_teacher: false,
                    is_host: false,
                    is_sub_client: false,
                    player_color: [0, 0, 0, 0],
                }],
            };
            // Send PlayerList FIRST
            client.send_packet(&ex_player_list).await;

            let ex_add_player = CAddPlayer {
                uuid: ex_profile.id,
                player_name: ex_profile.name.clone(),
                target_runtime_id: VarULong(existing_player.entity_id() as u64),
                platform_chat_id: String::new(),
                position: Vector3::new(ex_pos.x as f32, ex_pos.y as f32, ex_pos.z as f32),
                velocity: Vector3::new(ex_vel.x as f32, ex_vel.y as f32, ex_vel.z as f32),
                rotation: Vector2::new(ex_entity.pitch.load(), ex_entity.yaw.load()),
                y_head_rotation: ex_entity.head_yaw.load(),
                carried_item: NetworkItemStackDescriptor::default(),
                player_game_type: existing_player.gamemode.load().into(),
                entity_data: ex_entity.bedrock_metadata(),
                synced_properties: PropertySyncData::default(),
                abilities_data: pumpkin_protocol::bedrock::client::SerializedAbilitiesData {
                    target_player_raw_id: existing_player.entity_id() as i64,
                    player_permissions:
                        pumpkin_protocol::bedrock::client::PlayerPermissionLevel::Visitor,
                    command_permissions:
                        pumpkin_protocol::bedrock::client::CommandPermissionLevel::Any,
                    layers: vec![
                        pumpkin_protocol::bedrock::client::SerializedAbilitiesDataSerializedLayer {
                            serialized_layer: 0,
                            abilities_set: 0,
                            ability_value: 0,
                            fly_speed: 0.05,
                            vertical_fly_speed: 0.05,
                            walk_speed: 0.1,
                        },
                    ],
                },
                actor_links: Vec::new(),
                device_id: String::new(),
                build_platform: BuildPlatform::Unknown,
            };

            client.send_packet(&ex_add_player).await;

            let ex_held_item = existing_player.inventory().held_item();

            let ex_be_mob_equipment = pumpkin_protocol::bedrock::client::CMobEquipment {
                target_runtime_id: (existing_player.entity_id() as u64).into(),
                item: (&ex_held_item).into(),
                slot: 0,
                selected_slot: 0,
                container_id: 0,
            };

            client.send_packet(&ex_be_mob_equipment).await;
        }

        player.has_played_before.store(true, Ordering::Relaxed);

        // 3. Trigger Join Event and Broadcast Join Message
        let msg_comp = TextComponent::translate_cross(
            translation::java::MULTIPLAYER_PLAYER_JOINED,
            translation::bedrock::MULTIPLAYER_PLAYER_JOINED,
            [TextComponent::text(player.gameprofile.name.clone())],
        )
        .color_named(NamedColor::Yellow);

        let mut event = PlayerJoinEvent::new(player.clone(), msg_comp);
        server.plugin_manager.fire(server, &mut event).await;

        if !event.cancelled {
            self.broadcast_system_message(&event.join_message, false);
            info!("{}", event.join_message.to_pretty_console());
        }
    }

    #[expect(clippy::too_many_lines)]
    pub async fn spawn_java_player(
        &self,
        base_config: &BasicConfiguration,
        player: &Arc<Player>,
        server: &Arc<Server>,
    ) {
        let dimensions: Vec<ResourceLocation> = server
            .dimensions
            .iter()
            .map(|d| ResourceLocation::from(d.minecraft_name))
            .collect();

        // This code follows the vanilla packet order
        let entity_id = player.entity_id();
        let gamemode = player.gamemode.load();
        debug!(
            "spawning player {}, entity id {}",
            player.gameprofile.name, entity_id
        );

        let Some(client) = player.client.java() else {
            return;
        };
        // Send the login packet for our new player
        client
            .send_packet(&CLogin::new(
                entity_id,
                base_config.hardcore,
                &dimensions,
                server
                    .advanced_config
                    .networking
                    .java
                    .max_players
                    .try_into()
                    .unwrap_or(u16::MAX.into()),
                server
                    .advanced_config
                    .networking
                    .java
                    .view_distance
                    .get()
                    .into(), //  TODO: view distance
                server
                    .advanced_config
                    .networking
                    .java
                    .simulation_distance
                    .get()
                    .into(), // TODO: sim view dinstance
                false,
                true,
                false,
                PlayerSpawnData::new(
                    self.dimension.clone(),
                    biome::hash_seed(self.level.seed.0), // seed
                    gamemode as u8,
                    player
                        .previous_gamemode
                        .load()
                        .map_or(-1, |gamemode| gamemode as i8),
                    false,
                    false,
                    None,
                    VarInt(player.get_entity().portal_cooldown.load(Ordering::Relaxed) as i32),
                    self.sea_level.into(),
                ),
                server.advanced_config.networking.java.online_mode,
                // This should stay true even when reports are disabled.
                // It prevents the annoying popup when joining the server.
                true,
            ))
            .await;

        self.pair_new_player_with_tracked_entities(player);

        // Send the current ticking state to the new player so they are in sync.
        server.tick_rate_manager.update_joining_player(player).await;

        // Permissions, i.e. the commands a player may use.
        player.send_permission_lvl_update();

        // Difficulty of the world
        player.send_difficulty_update();
        {
            let command_dispatcher = server.command_dispatcher.load();

            client_suggestions::send_c_commands_packet(player, server, &command_dispatcher);
        };
        if client.version.load() < JavaMinecraftVersion::V_1_20_2
            && client.version.load() >= JavaMinecraftVersion::V_1_13
        {
            let version = client.version.load();
            let mut tags = Vec::new();
            for &key in pumpkin_data::tag::RegistryKey::NETWORK_KEYS {
                if pumpkin_data::tag::get_registry_key_tags(version, key)
                    .is_some_and(|map| !map.is_empty())
                {
                    tags.push(key);
                }
            }
            let packet = pumpkin_protocol::java::client::play::CUpdateTagsPlay::new(&tags);
            if let Ok(packet_data) = JavaClient::serialize_packet_for_version(&packet, version) {
                client.send_packet_now(packet_data).await;
            }
        }

        let (position, yaw, pitch) = if player.has_played_before.load(Ordering::Relaxed) {
            let position = player.position();
            let yaw = player.get_entity().yaw.load(); //info.spawn_angle;
            let pitch = player.get_entity().pitch.load();

            (position, yaw, pitch)
        } else {
            let info = &self.level_info.load();
            let spawn_position = Vector2::new(info.spawn_x, info.spawn_z);
            let chunk_pos = Vector2::new(info.spawn_x >> 4, info.spawn_z >> 4);
            self.level.get_or_fetch_chunk(chunk_pos, |_| ()).await;
            let top = self.get_top_block(spawn_position);
            let pos_y = if top > self.dimension.min_y {
                top + 1
            } else {
                info.spawn_y
            };

            let position = Vector3::new(
                f64::from(info.spawn_x) + 0.5,
                f64::from(pos_y),
                f64::from(info.spawn_z) + 0.5,
            );
            (position, info.spawn_yaw, info.spawn_pitch)
        };

        // Load chunks around the real spawn position before teleporting the client there.
        player.living_entity.entity.set_pos(position);
        player.living_entity.entity.set_rotation(yaw, pitch);
        player.living_entity.entity.last_pos.store(position);
        chunker::update_position(player);

        let center_chunk = player.living_entity.entity.chunk_pos.load();
        let chunk = self
            .level
            .get_or_fetch_chunk(center_chunk, std::clone::Clone::clone)
            .await;
        if let Some(server) = self.server.upgrade() {
            let mut event =
                crate::plugin::world::chunk_send::ChunkSend::new(player.world(), chunk.clone());
            server.plugin_manager.fire(&server, &mut event).await;
            if event.cancelled {
                return;
            }
        }
        client.send_chunks(&[chunk]).await;

        let velocity = player.living_entity.entity.velocity.load();

        debug!("Sending player teleport to {}", player.gameprofile.name);
        player.request_teleport(position, yaw, pitch);
        player.enqueue_waypoint_tracks();

        let gameprofile = &player.gameprofile;
        let bedrock_player_list = CPlayerList {
            action: CPlayerList::ACTION_ADD,
            entries: vec![PlayerListEntry {
                uuid: gameprofile.id,
                entity_unique_id: VarLong(entity_id as i64),
                username: gameprofile.name.clone(),
                xuid: String::new(),
                platform_chat_id: String::new(),
                build_platform: BuildPlatform::Unknown,
                skin: (**player.bedrock_skin.load()).clone(),
                is_teacher: false,
                is_host: false,
                is_sub_client: false,
                player_color: [0, 0, 0, 0],
            }],
        };

        let player_actions = [
            PlayerAction::AddPlayer {
                name: &gameprofile.name,
                properties: &gameprofile.properties.load(),
            },
            PlayerAction::UpdateGameMode(VarInt(gamemode as i32)),
            PlayerAction::UpdateListed(true),
            PlayerAction::UpdateLatency(VarInt(0)),
            PlayerAction::UpdateListOrder(VarInt(0)),
            PlayerAction::UpdateHat(true),
        ];
        let java_player = [pumpkin_protocol::java::client::play::Player {
            uuid: gameprofile.id,
            actions: &player_actions,
        }];
        let player_info_update = CPlayerInfoUpdate::new(
            (PlayerInfoFlags::ADD_PLAYER
                | PlayerInfoFlags::UPDATE_GAME_MODE
                | PlayerInfoFlags::UPDATE_LISTED
                | PlayerInfoFlags::UPDATE_LATENCY
                | PlayerInfoFlags::UPDATE_LIST_PRIORITY
                | PlayerInfoFlags::UPDATE_HAT)
                .bits(),
            &java_player,
        );

        self.broadcast_editioned(&player_info_update, &bedrock_player_list);

        // If the player has a custom tab_list_name, send an update for it
        if let Some(tab_list_name) = player.get_tab_list_name() {
            let actions = [PlayerAction::UpdateDisplayName(Some(&tab_list_name))];
            let java_player = [pumpkin_protocol::java::client::play::Player {
                uuid: gameprofile.id,
                actions: &actions,
            }];
            self.broadcast_packet_all(&CPlayerInfoUpdate::new(
                PlayerInfoFlags::UPDATE_DISPLAY_NAME.bits(),
                &java_player,
            ));
        }

        // Here, we send all the infos of players who already joined.
        let mut players_tab_list_names = Vec::new();
        {
            let players = self.players.load();
            let mut data_to_process = Vec::new();
            for p in players
                .iter()
                .filter(|p| p.gameprofile.id != player.gameprofile.id)
            {
                let props_guard = p.gameprofile.properties.load();
                data_to_process.push((props_guard, p));
            }

            let mut current_player_data = Vec::new();
            for (properties, player) in &data_to_process {
                let chat_session = player
                    .chat_session
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let tab_list_name = player.get_tab_list_name();

                let mut player_actions = vec![PlayerAction::AddPlayer {
                    name: &player.gameprofile.name,
                    properties,
                }];

                if base_config.allow_chat_reports {
                    let initialized = chat_session.session_id != uuid::Uuid::nil()
                        && !chat_session.public_key.is_empty()
                        && !chat_session.signature.is_empty();
                    player_actions.push(PlayerAction::InitializeChat(initialized.then(|| {
                        InitChat {
                            session_id: chat_session.session_id,
                            expires_at: chat_session.expires_at,
                            public_key: chat_session.public_key.clone(),
                            signature: chat_session.signature.clone(),
                        }
                    })));
                }

                player_actions.extend([
                    PlayerAction::UpdateGameMode(VarInt(player.gamemode.load() as i32)),
                    PlayerAction::UpdateListed(player.tab_list_listed.load(Ordering::Relaxed)),
                    PlayerAction::UpdateLatency(VarInt(
                        player.tab_list_latency.load(Ordering::Relaxed),
                    )),
                    PlayerAction::UpdateListOrder(VarInt(
                        player.tab_list_order.load(Ordering::Relaxed),
                    )),
                    PlayerAction::UpdateHat(true),
                ]);
                drop(chat_session);

                current_player_data.push((&player.gameprofile.id, player_actions));

                // Collect tab_list_names for sending later
                if tab_list_name.is_some() {
                    players_tab_list_names.push((player.gameprofile.id, tab_list_name));
                }
            }

            let mut action_flags = PlayerInfoFlags::ADD_PLAYER
                | PlayerInfoFlags::UPDATE_LISTED
                | PlayerInfoFlags::UPDATE_LATENCY
                | PlayerInfoFlags::UPDATE_LIST_PRIORITY
                | PlayerInfoFlags::UPDATE_GAME_MODE
                | PlayerInfoFlags::UPDATE_HAT;
            if base_config.allow_chat_reports {
                action_flags |= PlayerInfoFlags::INITIALIZE_CHAT;
            }

            let entries = current_player_data
                .iter()
                .map(|(id, actions)| java::client::play::Player {
                    uuid: **id,
                    actions,
                })
                .collect::<Vec<_>>();

            debug!("Sending player info to {}", player.gameprofile.name);
            client
                .enqueue_client_packet(&CPlayerInfoUpdate::new(action_flags.bits(), &entries))
                .await;

            // Send tab_list_names for existing players with custom names
            for (player_id, tab_list_name) in &players_tab_list_names {
                if let Some(name) = tab_list_name {
                    let actions = [PlayerAction::UpdateDisplayName(Some(name))];
                    let java_player = [pumpkin_protocol::java::client::play::Player {
                        uuid: *player_id,
                        actions: &actions,
                    }];
                    client
                        .enqueue_client_packet(&CPlayerInfoUpdate::new(
                            PlayerInfoFlags::UPDATE_DISPLAY_NAME.bits(),
                            &java_player,
                        ))
                        .await;
                }
            }
        };

        let gameprofile = &player.gameprofile;

        let bedrock_add_player = CAddPlayer {
            uuid: gameprofile.id,
            player_name: gameprofile.name.clone(),
            target_runtime_id: VarULong(entity_id as u64),
            platform_chat_id: String::new(),
            position: Vector3::new(position.x as f32, position.y as f32, position.z as f32),
            velocity: Vector3::new(velocity.x as f32, velocity.y as f32, velocity.z as f32),
            rotation: Vector2::new(pitch, yaw),
            y_head_rotation: yaw,
            carried_item: NetworkItemStackDescriptor::default(),
            player_game_type: player.gamemode.load().into(),
            entity_data: player.get_entity().bedrock_metadata(),
            synced_properties: PropertySyncData::default(),
            abilities_data: pumpkin_protocol::bedrock::client::SerializedAbilitiesData {
                target_player_raw_id: entity_id as i64,
                player_permissions:
                    pumpkin_protocol::bedrock::client::PlayerPermissionLevel::Visitor,
                command_permissions: pumpkin_protocol::bedrock::client::CommandPermissionLevel::Any,
                layers: vec![
                    pumpkin_protocol::bedrock::client::SerializedAbilitiesDataSerializedLayer {
                        serialized_layer: 0,
                        abilities_set: 0,
                        ability_value: 0,
                        fly_speed: 0.05,
                        vertical_fly_speed: 0.05,
                        walk_speed: 0.1,
                    },
                ],
            },
            actor_links: Vec::new(),
            device_id: String::new(),
            build_platform: BuildPlatform::Unknown,
        };

        // Spawn the player for every client.
        let spawn_entity = CSpawnEntity::new(
            entity_id.into(),
            gameprofile.id,
            i32::from(EntityType::PLAYER.id).into(),
            position,
            pitch,
            yaw,
            yaw,
            0.into(),
            velocity,
        );

        self.broadcast_packet_except_editioned(
            &[player.gameprofile.id],
            &spawn_entity,
            &bedrock_add_player,
        );

        // Broadcast metadata to Java players so they can correctly interact with the new player
        let skin_parts = player.config.load().skin_parts;

        self.broadcast_skin_parts(
            &[gameprofile.id],
            entity_id,
            skin_parts,
            &CSetActorData {
                target_runtime_id: VarULong(entity_id as u64),
                actor_data: player.get_entity().bedrock_metadata(),
                synced_properties: PropertySyncData {
                    int_entries_list: HashMap::new(),
                    float_entries_list: HashMap::new(),
                },
                tick: VarULong(0),
            },
        );

        // Spawn players for our client.
        let id = player.gameprofile.id;
        for existing_player in self
            .players
            .load()
            .iter()
            .filter(|c| c.gameprofile.id != id)
        {
            let entity = &existing_player.get_entity();
            let pos = entity.pos.load();
            let gameprofile = &existing_player.gameprofile;
            let bedrock_add_player = CAddPlayer {
                uuid: gameprofile.id,
                player_name: gameprofile.name.clone(),
                target_runtime_id: VarULong(existing_player.entity_id() as u64),
                platform_chat_id: String::new(),
                position: Vector3::new(pos.x as f32, pos.y as f32, pos.z as f32),
                velocity: Vector3::new(
                    entity.velocity.load().x as f32,
                    entity.velocity.load().y as f32,
                    entity.velocity.load().z as f32,
                ),
                rotation: Vector2::new(entity.pitch.load(), entity.yaw.load()),
                y_head_rotation: entity.head_yaw.load(),
                carried_item: NetworkItemStackDescriptor::default(),
                player_game_type: existing_player.gamemode.load().into(),
                entity_data: entity.bedrock_metadata(),
                synced_properties: PropertySyncData::default(),
                abilities_data: pumpkin_protocol::bedrock::client::SerializedAbilitiesData {
                    target_player_raw_id: existing_player.entity_id() as i64,
                    player_permissions:
                        pumpkin_protocol::bedrock::client::PlayerPermissionLevel::Visitor,
                    command_permissions:
                        pumpkin_protocol::bedrock::client::CommandPermissionLevel::Any,
                    layers: vec![
                        pumpkin_protocol::bedrock::client::SerializedAbilitiesDataSerializedLayer {
                            serialized_layer: 0,
                            abilities_set: 0,
                            ability_value: 0,
                            fly_speed: 0.05,
                            vertical_fly_speed: 0.05,
                            walk_speed: 0.1,
                        },
                    ],
                },
                actor_links: Vec::new(),
                device_id: String::new(),
                build_platform: BuildPlatform::Unknown,
            };

            let bedrock_player_list = CPlayerList {
                action: CPlayerList::ACTION_ADD,
                entries: vec![PlayerListEntry {
                    uuid: gameprofile.id,
                    entity_unique_id: VarLong(existing_player.entity_id() as i64),
                    username: gameprofile.name.clone(),
                    xuid: String::new(),
                    platform_chat_id: String::new(),
                    build_platform: BuildPlatform::Unknown,
                    skin: (**existing_player.bedrock_skin.load()).clone(),
                    is_teacher: false,
                    is_host: false,
                    is_sub_client: false,
                    player_color: [0, 0, 0, 0],
                }],
            };

            let actions = [
                PlayerAction::AddPlayer {
                    name: &gameprofile.name,
                    properties: &gameprofile.properties.load(),
                },
                PlayerAction::UpdateGameMode(VarInt(existing_player.gamemode.load() as i32)),
                PlayerAction::UpdateListed(existing_player.tab_list_listed.load(Ordering::Relaxed)),
                PlayerAction::UpdateLatency(VarInt(
                    existing_player.tab_list_latency.load(Ordering::Relaxed),
                )),
                PlayerAction::UpdateListOrder(VarInt(
                    existing_player.tab_list_order.load(Ordering::Relaxed),
                )),
                PlayerAction::UpdateHat(true),
            ];
            let java_player = [pumpkin_protocol::java::client::play::Player {
                uuid: gameprofile.id,
                actions: &actions,
            }];
            player
                .client
                .enqueue_packet_editioned(
                    &CPlayerInfoUpdate::new(
                        (PlayerInfoFlags::ADD_PLAYER
                            | PlayerInfoFlags::UPDATE_LISTED
                            | PlayerInfoFlags::UPDATE_GAME_MODE
                            | PlayerInfoFlags::UPDATE_LATENCY
                            | PlayerInfoFlags::UPDATE_LIST_PRIORITY
                            | PlayerInfoFlags::UPDATE_HAT)
                            .bits(),
                        &java_player,
                    ),
                    &bedrock_player_list,
                )
                .await;

            player
                .client
                .enqueue_packet_editioned(
                    &CSpawnEntity::new(
                        existing_player.entity_id().into(),
                        gameprofile.id,
                        i32::from(EntityType::PLAYER.id).into(),
                        pos,
                        entity.pitch.load(),
                        entity.yaw.load(),
                        entity.head_yaw.load(),
                        0.into(),
                        entity.velocity.load(),
                    ),
                    &bedrock_add_player,
                )
                .await;

            if client.version.load() >= JavaMinecraftVersion::V_1_21 {
                let config = existing_player.config.load();
                let mut buf = Vec::new();
                {
                    let meta = Metadata::new(
                        pumpkin_data::tracked_data::player::PLAYER_MODE_CUSTOMISATION,
                        config.skin_parts,
                    );
                    let _ = meta.write(&mut buf, &client.version.load());
                };
                {
                    let meta = Metadata::new(
                        pumpkin_data::tracked_data::player::PLAYER_MODE_CUSTOMIZATION_ID,
                        config.skin_parts,
                    );
                    let _ = meta.write(&mut buf, &client.version.load());
                };
                drop(config);
                // END
                buf.put_u8(255);
                client
                    .enqueue_client_packet(&CSetEntityMetadata::new(
                        existing_player.get_entity().entity_id.into(),
                        buf.into(),
                    ))
                    .await;
            }

            {
                let held_item = existing_player.inventory.held_item();
                let equipment_list = {
                    let mut equipment_list =
                        vec![(EquipmentSlot::MAIN_HAND.discriminant(), held_item.clone())];

                    let equipment_guard = existing_player
                        .inventory
                        .entity_equipment
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    for (slot, item_stack) in &equipment_guard.equipment {
                        equipment_list.push((slot.discriminant(), item_stack.clone()));
                    }
                    equipment_list
                };

                let equipment: Vec<(i8, ItemStackSerializer)> = equipment_list
                    .iter()
                    .map(|(slot, stack)| (*slot, ItemStackSerializer::from(stack.clone())))
                    .collect();

                let je_packet = CSetEquipment::new(existing_player.entity_id().into(), equipment);

                let be_mob_equipment = pumpkin_protocol::bedrock::client::CMobEquipment {
                    target_runtime_id: (existing_player.entity_id() as u64).into(),
                    item: (&held_item).into(),
                    slot: 0,
                    selected_slot: 0,
                    container_id: 0,
                };

                player
                    .client
                    .enqueue_packet_editioned(&je_packet, &be_mob_equipment)
                    .await;
            }
        }
        player.send_client_information();

        player.send_abilities_update();

        // Sync selected slot
        player.enqueue_set_held_item_packet(&CSetSelectedSlot::new(
            player.get_inventory().get_selected_slot() as i8,
        ));

        if client.version.load() >= JavaMinecraftVersion::V_1_20_2 {
            // Start waiting for level chunks. Sets the "Loading Terrain" screen (Added in 1.20.2)
            debug!("Sending waiting chunks to {}", player.gameprofile.name);
            client
                .send_packet(&CGameEvent::new(GameEvent::StartWaitingChunks, 0.0))
                .await;
        }

        self.worldborder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .init_client(client);

        // Sends initial time
        player.send_time(self);

        // Sends initial scoreboard state
        player.send_scoreboard();

        let (spawn_block_pos, yaw, pitch) = {
            let level_info_lock = self.level_info.load();
            (
                BlockPos::new(
                    level_info_lock.spawn_x,
                    level_info_lock.spawn_y,
                    level_info_lock.spawn_z,
                ),
                level_info_lock.spawn_yaw,
                level_info_lock.spawn_pitch,
            )
        };

        client
            .send_packet(&CPlayerSpawnPosition::new(
                spawn_block_pos,
                yaw,
                pitch,
                self.dimension.minecraft_name.to_owned(),
            ))
            .await;

        // Send initial weather state
        let (is_raining, rain_level, thunder_level) = {
            let weather = self
                .weather
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                weather.raining,
                weather.rain_level.clamp(0.0, 1.0),
                weather.thunder_level.clamp(0.0, 1.0),
            )
        };
        if is_raining {
            client
                .enqueue_client_packet(&CGameEvent::new(GameEvent::BeginRaining, 0.0))
                .await;

            client
                .enqueue_client_packet(&CGameEvent::new(GameEvent::RainLevelChange, rain_level))
                .await;
            client
                .enqueue_client_packet(&CGameEvent::new(
                    GameEvent::ThunderLevelChange,
                    thunder_level,
                ))
                .await;
        }

        let player_bossbars = server
            .bossbars
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_player_bars(&player.gameprofile.id)
            .map(|bars| bars.into_iter().cloned().collect::<Vec<_>>());
        if let Some(bossbars) = player_bossbars {
            for bossbar in &bossbars {
                player.send_bossbar(bossbar);
            }
        }

        player.has_played_before.store(true, Ordering::Relaxed);
        player.on_screen_handler_opened(&player.player_screen_handler);

        player.send_active_effects();
        player.breath_manager.send_air_supply(player);
        self.send_player_equipment(player);
        player
            .living_entity
            .send_current_equipment_attribute_modifiers();

        if let crate::net::ClientPlatform::Java(java_client) = player.client.as_ref() {
            if java_client.version.load() == JavaMinecraftVersion::V_26_2 {
                let update_recipes = CUpdateRecipes::generated_vanilla();
                if let Ok(data) = java_client.serialize_packet(&update_recipes) {
                    java_client.send_packet_now(data).await;
                }
            }
            if server.advanced_config.recipe.send_recipes
                && java_client.version.load() >= JavaMinecraftVersion::V_1_21_2
            {
                let settings_packet = CRecipeBookSettings::default_closed();
                if let Ok(data) = java_client.serialize_packet(&settings_packet) {
                    java_client.send_packet_now(data).await;
                }
                let dynamic_recipes = server.recipe_manager.get_dynamic_recipes();
                let add_packet = CRecipeBookAdd::new(true, &dynamic_recipes);
                if let Ok(data) = java_client.serialize_packet(&add_packet) {
                    java_client.send_packet_now(data).await;
                }
            }
        }
        let msg_comp = TextComponent::translate_cross(
            translation::java::MULTIPLAYER_PLAYER_JOINED,
            translation::bedrock::MULTIPLAYER_PLAYER_JOINED,
            [TextComponent::text(player.gameprofile.name.clone())],
        )
        .color_named(NamedColor::Yellow);
        let mut event = PlayerJoinEvent::new(player.clone(), msg_comp);

        server.plugin_manager.fire(server, &mut event).await;

        if !event.cancelled {
            self.broadcast_system_message(&event.join_message, false);
            // TODO: Switch to structured logging, e.g. info!(player = %name, "connected")
            info!("{}", event.join_message.to_pretty_console());
        }
    }

    fn send_player_equipment(&self, from: &Player) {
        let held_item = from.inventory.held_item();
        let mut equipment_list = vec![(EquipmentSlot::MAIN_HAND.discriminant(), held_item.clone())];

        let equipment_guard = from
            .inventory
            .entity_equipment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (slot, item_stack) in &equipment_guard.equipment {
            equipment_list.push((slot.discriminant(), item_stack.clone()));
        }
        drop(equipment_guard);

        let equipment: Vec<(i8, ItemStackSerializer)> = equipment_list
            .iter()
            .map(|(slot, stack)| (*slot, ItemStackSerializer::from(stack.clone())))
            .collect();
        let je_packet = CSetEquipment::new(from.entity_id().into(), equipment);

        let be_mob_equipment = pumpkin_protocol::bedrock::client::CMobEquipment {
            target_runtime_id: (from.entity_id() as u64).into(),
            item: (&held_item).into(),
            slot: 0,
            selected_slot: 0,
            container_id: 0,
        };

        self.send_to_tracking_players_editioned(from.get_entity(), &je_packet, &be_mob_equipment);
    }

    pub fn send_world_info(
        &self,
        player: &Arc<Player>,
        position: Vector3<f64>,
        yaw: f32,
        pitch: f32,
    ) {
        if let ClientPlatform::Java(client) = player.client.as_ref() {
            self.worldborder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .init_client(client);
        }

        // TODO: World spawn (compass stuff)

        if let ClientPlatform::Java(client) = player.client.as_ref()
            && client.version.load() >= JavaMinecraftVersion::V_1_20_2
        {
            player.try_send_client_packet(&CGameEvent::new(GameEvent::StartWaitingChunks, 0.0));
        }

        let entity = &player.get_entity();

        self.broadcast_packet_except(
            &[player.gameprofile.id],
            // TODO: add velo
            &CSpawnEntity::new(
                entity.entity_id.into(),
                player.gameprofile.id,
                i32::from(EntityType::PLAYER.id).into(),
                position,
                pitch,
                yaw,
                yaw,
                0.into(),
                Vector3::new(0.0, 0.0, 0.0),
            ),
        );

        player.send_client_information();

        chunker::update_position(player);
        // Update commands

        player.set_health(20.0);
    }
}
