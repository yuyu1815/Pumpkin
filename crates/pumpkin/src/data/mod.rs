use std::{env, fs, path::Path, sync::RwLock};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tracing::{debug, error, warn};

const DATA_FOLDER: &str = "data/";

pub mod op;

fn deserialize_strict_entries<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Vec::<T>::deserialize(deserializer)
}

fn deserialize_tolerant_entries<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    let values = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .enumerate()
        .filter_map(|(index, value)| match serde_json::from_value(value) {
            Ok(entry) => Some(entry),
            Err(_) => {
                warn!(entry_index = index, "Skipping invalid ops/whitelist entry");
                None
            }
        })
        .collect())
}

pub mod advancement_data;
pub mod banlist_serializer;
pub mod banned_ip;
pub mod banned_player;
pub mod datapack;
pub mod player_server;
pub mod usercache;
pub mod whitelist;

pub struct VanillaData {
    pub banned_ip_list: RwLock<banned_ip::BannedIpList>,
    pub banned_player_list: RwLock<banned_player::BannedPlayerList>,
    pub operator_config: RwLock<op::OperatorConfig>,
    pub user_cache: RwLock<usercache::UserCache>,
    pub whitelist_config: RwLock<whitelist::WhitelistConfig>,
}

impl VanillaData {
    pub fn load() -> Result<Self, String> {
        Ok(Self {
            banned_ip_list: RwLock::new(banned_ip::BannedIpList::load_strict()?),
            banned_player_list: RwLock::new(banned_player::BannedPlayerList::load_strict()?),
            operator_config: RwLock::new(op::OperatorConfig::load_strict()?),
            // User cache parsing intentionally retains its existing best-effort behavior.
            user_cache: RwLock::new(usercache::UserCache::load()),
            whitelist_config: RwLock::new(whitelist::WhitelistConfig::load_strict()?),
        })
    }
}

pub trait LoadJSONConfiguration {
    fn load_strict() -> Result<Self, String>
    where
        Self: Sized + Default + Serialize + for<'de> Deserialize<'de>,
    {
        let data_dir = env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(DATA_FOLDER);
        if !data_dir.exists() {
            fs::create_dir_all(&data_dir).map_err(|err| {
                format!("Couldn't create data directory {}: {err}", data_dir.display())
            })?;
        }
        load_strict_from_path(&data_dir.join(Self::get_path()))
    }

    #[must_use]
    fn load() -> Self
    where
        Self: Sized + Default + Serialize + for<'de> Deserialize<'de>,
    {
        let exe_dir = env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let data_dir = exe_dir.join(DATA_FOLDER);
        if !data_dir.exists() {
            debug!("creating new data root folder");
            let _ = fs::create_dir(&data_dir);
        }
        let path = data_dir.join(Self::get_path());

        let config = if path.exists() {
            let file_content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(err) => {
                    error!(
                        "Couldn't read configuration file at {}: {err}",
                        path.display()
                    );
                    return Self::default();
                }
            };

            match serde_json::from_str(&file_content) {
                Ok(c) => c,
                Err(err) => {
                    error!(
                        "Couldn't parse data config at {}. Reason: {err}. Falling back to default.",
                        path.display()
                    );
                    Self::default()
                }
            }
        } else {
            let content = Self::default();

            if let Ok(json_str) = serde_json::to_string_pretty(&content) {
                let _ = fs::write(&path, json_str);
            }

            content
        };

        config.validate();
        config
    }

    fn get_path() -> &'static Path;

    fn validate(&self);
}

fn load_strict_from_path<T>(path: &Path) -> Result<T, String>
where
    T: Default + Serialize + for<'de> Deserialize<'de> + LoadJSONConfiguration,
{
    let config = match fs::read_to_string(path) {
        Ok(file_content) => serde_json::from_str(&file_content)
            .map_err(|err| format!("Couldn't parse data config at {}: {err}", path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let content = T::default();
            let json_str = serde_json::to_string_pretty(&content).map_err(|err| {
                format!("Couldn't serialize default configuration for {}: {err}", path.display())
            })?;
            fs::write(path, json_str).map_err(|err| {
                format!("Couldn't create configuration file at {}: {err}", path.display())
            })?;
            content
        }
        Err(err) => {
            return Err(format!(
                "Couldn't read configuration file at {}: {err}",
                path.display()
            ));
        }
    };
    config.validate();
    Ok(config)
}

pub trait SaveJSONConfiguration: LoadJSONConfiguration {
    fn save(&self)
    where
        Self: Sized + Default + Serialize + for<'de> Deserialize<'de>,
    {
        let exe_dir = env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let data_dir = exe_dir.join(DATA_FOLDER);
        if !data_dir.exists() {
            debug!("creating new data root folder");
            let _ = fs::create_dir(&data_dir);
        }
        let path = data_dir.join(Self::get_path());

        let content = match serde_json::to_string_pretty(self) {
            Ok(content) => content,
            Err(err) => {
                warn!(
                    "Couldn't serialize operator data config to {}. Reason: {err}",
                    path.display()
                );
                return;
            }
        };

        if let Err(err) = std::fs::write(&path, content) {
            warn!(
                "Couldn't write operator config to {}. Reason: {err}",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        banned_ip::BannedIpList, banned_player::BannedPlayerList, op::OperatorConfig,
        load_strict_from_path, whitelist::WhitelistConfig,
    };

    #[test]
    fn ops_and_whitelist_skip_invalid_entries_but_warn() {
        let ops: OperatorConfig = serde_json::from_value(json!([
            {"uuid":"not-a-uuid","name":"bad","level":4,"bypassesPlayerLimit":true},
            {"uuid":"00000000-0000-0000-0000-000000000001","name":"Op","level":4,"bypassesPlayerLimit":true}
        ])).unwrap();
        assert_eq!(ops.ops.len(), 1);
        assert!(ops.ops[0].bypasses_player_limit);
        assert!(
            serde_json::to_value(&ops).unwrap()[0]
                .get("bypassesPlayerLimit")
                .is_some()
        );
        let native_op: OperatorConfig = serde_json::from_value(json!([
            {"uuid":"00000000-0000-0000-0000-000000000005","name":"Native","level":2,"bypasses_player_limit":false}
        ])).unwrap();
        assert_eq!(native_op.ops.len(), 1);

        let whitelist: WhitelistConfig = serde_json::from_value(json!([
            {"uuid":"bad","name":"bad"},
            {"uuid":"00000000-0000-0000-0000-000000000002","name":"Allowed"}
        ]))
        .unwrap();
        assert_eq!(whitelist.whitelist.len(), 1);

        let banned_players: BannedPlayerList = serde_json::from_value(json!([
            {"uuid":"00000000-0000-0000-0000-000000000003","name":"Banned","created":"2026-01-02 03:04:05 +0000","source":"Server","expires":"forever","reason":"Test"},
            {"uuid":"00000000-0000-0000-0000-000000000006","name":"Native","created":"2026-01-02 03:04:05+00:00","source":"Server","expires":"forever","reason":"Test"}
        ])).unwrap();
        assert_eq!(banned_players.banned_players.len(), 2);
        let dates: Vec<_> = banned_players
            .banned_players
            .iter()
            .map(|entry| entry.created.unix_timestamp())
            .collect();
        assert_eq!(dates[0], dates[1]);
        let serialized = serde_json::to_value(&banned_players).unwrap();
        let round_trip: BannedPlayerList = serde_json::from_value(serialized).unwrap();
        assert_eq!(round_trip.banned_players[0].created.unix_timestamp(), dates[0]);

        let banned_ips: BannedIpList = serde_json::from_value(json!([
            {"ip":"192.0.2.1","created":"2026-01-02 03:04:05 +0000","source":"Server","expires":"forever","reason":"Test"}
        ])).unwrap();
        assert_eq!(banned_ips.banned_ips.len(), 1);
        assert!(serde_json::from_value::<BannedIpList>(json!([
            {"ip":"invalid"},
            {"ip":"192.0.2.1","created":"2026-01-02 03:04:05 +0000","source":"Server","expires":"forever","reason":"Test"}
        ])).is_err());
        assert!(serde_json::from_value::<BannedPlayerList>(json!([
            {"uuid":"bad"},
            {"uuid":"00000000-0000-0000-0000-000000000003","name":"Banned","created":"2026-01-02 03:04:05 +0000","source":"Server","expires":"forever","reason":"Test"}
        ])).is_err());
    }

    #[test]
    fn corrupt_existing_security_list_fails_without_overwriting_it() {
        let path = std::env::temp_dir().join(format!(
            "pumpkin-security-list-{}.json",
            uuid::Uuid::new_v4()
        ));
        let corrupt = "{ definitely not valid json";
        std::fs::write(&path, corrupt).unwrap();

        assert!(load_strict_from_path::<WhitelistConfig>(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn malformed_ban_rows_fail_strict_load_without_overwriting_source() {
        let content = r#"[{"uuid":"00000000-0000-0000-0000-000000000003","name":"Banned","created":"2026-01-02 03:04:05 +0000","source":"Server","expires":"forever","reason":"Test"},{"uuid":"bad"}]"#;
        for (filename, content) in [
            ("banned-players.json", content),
            ("banned-ips.json", r#"[{"ip":"192.0.2.1","created":"2026-01-02 03:04:05 +0000","source":"Server","expires":"forever","reason":"Test"},{"ip":"invalid","created":"2026-01-02 03:04:05 +0000","source":"Server","expires":"forever","reason":"Test"}]"#),
        ] {
            let path = std::env::temp_dir().join(format!(
                "pumpkin-{filename}-{}.json",
                uuid::Uuid::new_v4()
            ));
            std::fs::write(&path, content).unwrap();
            let error = if filename == "banned-players.json" {
                load_strict_from_path::<BannedPlayerList>(&path).err().unwrap()
            } else {
                load_strict_from_path::<BannedIpList>(&path).err().unwrap()
            };
            assert!(error.contains("Couldn't parse data config"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn missing_security_list_propagates_write_failure() {
        let parent = std::env::temp_dir().join(format!(
            "pumpkin-not-a-directory-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&parent, "fixture").unwrap();
        let path = parent.join("banned-players.json");
        let error = load_strict_from_path::<BannedPlayerList>(&path).err().unwrap();
        assert!(error.contains("Couldn't create configuration file"));
        assert_eq!(std::fs::read_to_string(&parent).unwrap(), "fixture");
        std::fs::remove_file(parent).unwrap();
    }

    #[test]
    fn malformed_top_level_list_is_a_parse_error() {
        for malformed in ["{}", "null", "not json"] {
            assert!(serde_json::from_str::<OperatorConfig>(malformed).is_err());
            assert!(serde_json::from_str::<WhitelistConfig>(malformed).is_err());
            assert!(serde_json::from_str::<BannedPlayerList>(malformed).is_err());
            assert!(serde_json::from_str::<BannedIpList>(malformed).is_err());
        }
    }
}
