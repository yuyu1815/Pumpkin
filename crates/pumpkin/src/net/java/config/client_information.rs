#[allow(clippy::wildcard_imports)]
use super::*;
use crate::{net::java::clamp_view_distance, server::Server};

impl JavaClient {
    pub async fn handle_client_information_config(
        &self,
        server: &Server,
        client_information: SClientInformationConfig<'_>,
    ) {
        debug!("Handling client settings");

        if let (Ok(main_hand), Ok(chat_mode)) = (
            Hand::try_from(client_information.main_hand.0),
            ChatMode::try_from(client_information.chat_mode.0),
        ) {
            self.config.store(Arc::new(PlayerConfig {
                locale: client_information.locale.to_string(),
                view_distance: clamp_view_distance(
                    client_information.view_distance,
                    server.advanced_config.networking.java.view_distance,
                ),
                chat_mode,
                chat_colors: client_information.chat_colors,
                skin_parts: client_information.skin_parts,
                main_hand,
                text_filtering: client_information.text_filtering,
                server_listing: client_information.server_listing,
            }));
        } else {
            self.kick(TextComponent::text("Invalid hand or chat type"))
                .await;
        }
    }
}
