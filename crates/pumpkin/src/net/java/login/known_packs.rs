#[allow(clippy::wildcard_imports)]
use super::*;
use crate::net::java::{KnownPacksSelection, registry_entries_for_known_packs};

impl PendingConnection {
    pub async fn handle_known_packs(&mut self, server: &Server) {
        self.handle_known_packs_with_selection(server, KnownPacksSelection::Fallback)
            .await;
    }

    pub(crate) async fn handle_known_packs_with_selection(
        &mut self,
        server: &Server,
        selection: KnownPacksSelection,
    ) {
        let version = self.version.load();
        if version.supports_configuration_state() {
            if version < JavaMinecraftVersion::V_1_20_5 {
                let features = server.get_enabled_features();
                self.send_packet_now(&CFeatureFlags::new(&features)).await;
            }
            let registry = pumpkin_data::registry::Registry::get_synced(version);
            for reg in &registry {
                let entries = registry_entries_for_known_packs(reg, selection);
                self.send_packet_now(&CRegistryData::new(&reg.registry_id, entries))
                    .await;
            }
        }
        let mut tags = Vec::new();
        for &key in pumpkin_data::tag::RegistryKey::NETWORK_KEYS {
            if pumpkin_data::tag::get_registry_key_tags(version, key)
                .is_some_and(|map| !map.is_empty())
            {
                tags.push(key);
            }
        }
        self.send_packet_now(&CUpdateTags::new(&tags)).await;
        self.configuration_phase
            .store(ConfigurationPhase::AwaitingFinishAck);
        self.send_packet_now(&CFinishConfig).await;
    }
}
