#[allow(clippy::wildcard_imports)]
use super::*;
use crate::net::java::claim_reconfiguration_ack;
use pumpkin_protocol::ConnectionState;

impl JavaClient {
    pub fn handle_configuration_acknowledged(&self, player: &Player) -> bool {
        if !claim_reconfiguration_ack(&self.configuration_phase) {
            warn!(
                "Ignoring unsolicited or duplicate configuration acknowledgment from player {}",
                player.gameprofile.name
            );
            self.try_kick(&TextComponent::text(
                "Unexpected configuration acknowledgment",
            ));
            return false;
        }

        debug!(
            "Player {} acknowledged configuration switch",
            player.gameprofile.name
        );
        self.connection_state.store(ConnectionState::Config);
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::net::java::ConfigurationPhase;

    #[test]
    fn unsolicited_configuration_ack_is_not_a_reconfiguration_transition() {
        assert_ne!(
            ConfigurationPhase::Play,
            ConfigurationPhase::ReconfigurationAwaitingAck
        );
        assert_ne!(
            ConfigurationPhase::AwaitingFinishAck,
            ConfigurationPhase::ReconfigurationAwaitingAck
        );
    }
}
