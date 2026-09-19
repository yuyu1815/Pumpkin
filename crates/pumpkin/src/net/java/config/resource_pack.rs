#[allow(clippy::wildcard_imports)]
use super::*;

impl JavaClient {
    pub async fn handle_resource_pack_response(
        &self,
        server: &Server,
        packet: SConfigResourcePack,
    ) {
        if !self.configuration_phase.load().accepts_resource_pack() {
            warn!(
                "Client {} returned a resource-pack response outside the pending phase",
                self.id
            );
            return;
        }

        let resource_config = &server.advanced_config.resource_pack.java;
        if resource_config.enabled {
            if super::super::resource_pack_uuid_matches(&resource_config.url, packet.uuid) {
                match packet.response_result() {
                    ResourcePackResponseResult::DownloadSuccess => {
                        trace!(
                            "Client {} successfully downloaded the resource pack",
                            self.id
                        );
                    }
                    ResourcePackResponseResult::DownloadFail => {
                        warn!(
                            "Client {} failed to downloaded the resource pack. Is it available on the internet?",
                            self.id
                        );
                    }
                    ResourcePackResponseResult::Downloaded => {
                        trace!("Client {} already has the resource pack", self.id);
                    }
                    ResourcePackResponseResult::Accepted => {
                        trace!("Client {} accepted the resource pack", self.id);
                    }
                    ResourcePackResponseResult::Declined => {
                        trace!("Client {} declined the resource pack", self.id);
                    }
                    ResourcePackResponseResult::InvalidUrl => {
                        warn!(
                            "Client {} reported that the resource pack URL is invalid!",
                            self.id
                        );
                    }
                    ResourcePackResponseResult::ReloadFailed => {
                        trace!("Client {} failed to reload the resource pack", self.id);
                    }
                    ResourcePackResponseResult::Discarded => {
                        trace!("Client {} discarded the resource pack", self.id);
                    }
                    ResourcePackResponseResult::Unknown(result) => {
                        warn!(
                            "Client {} responded with a bad result: {}!",
                            self.id, result
                        );
                    }
                }
            } else {
                warn!(
                    "Client {} returned a response for a resource pack we did not set!",
                    self.id
                );
                self.try_kick(&TextComponent::text("Unknown resource pack response"));
                return;
            }
        } else {
            warn!(
                "Client {} returned a response for a resource pack that was not enabled!",
                self.id
            );
            self.try_kick(&TextComponent::text(
                "Resource pack response is not expected",
            ));
            return;
        }

        match super::super::resource_pack_response_action(
            &packet.response_result(),
            resource_config.force,
        ) {
            super::super::ResourcePackResponseAction::Wait => {}
            super::super::ResourcePackResponseAction::Complete => {
                self.send_known_packs(server).await;
            }
            super::super::ResourcePackResponseAction::Kick(reason) => {
                self.try_kick(&TextComponent::text(reason));
            }
        }
    }

    pub async fn send_known_packs(&self, server: &Server) {
        self.configuration_phase
            .store(ConfigurationPhase::AwaitingKnownPacks);
        let features = server.get_enabled_features();
        self.send_packet(&CFeatureFlags::new(&features)).await;
        let version_str = self.version.load().to_string();
        let loaded_packs = server.datapack_manager.get_loaded_packs();
        let known_packs = server.get_known_packs(&version_str, &loaded_packs);
        self.remember_known_packs(&known_packs);
        self.send_packet(&CKnownPacks::new(&known_packs)).await;
    }
}
