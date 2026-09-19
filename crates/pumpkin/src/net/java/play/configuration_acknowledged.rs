#[allow(clippy::wildcard_imports)]
use super::*;
use crate::net::java::ConfigurationPhase;
use pumpkin_protocol::ConnectionState;

impl JavaClient {
    pub fn handle_configuration_acknowledged(&self, player: &Player) {
        if !self
            .configuration_phase
            .load()
            .accepts_reconfiguration_ack()
        {
            warn!(
                "Ignoring unsolicited configuration acknowledgment from player {}",
                player.gameprofile.name
            );
            self.try_kick(&TextComponent::text(
                "Unexpected configuration acknowledgment",
            ));
            return;
        }

        debug!(
            "Player {} acknowledged configuration switch",
            player.gameprofile.name
        );
        self.configuration_phase
            .store(ConfigurationPhase::AwaitingKnownPacks);
        self.connection_state.store(ConnectionState::Config);
    }
}

#[cfg(test)]
mod tests {
    use super::ConfigurationPhase;

    #[test]
    fn unsolicited_configuration_ack_is_not_a_reconfiguration_transition() {
        assert!(!ConfigurationPhase::Play.accepts_reconfiguration_ack());
        assert!(!ConfigurationPhase::AwaitingFinishAck.accepts_reconfiguration_ack());
        assert!(ConfigurationPhase::ReconfigurationAwaitingAck.accepts_reconfiguration_ack());
    }
}
