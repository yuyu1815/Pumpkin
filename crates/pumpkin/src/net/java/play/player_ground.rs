#[allow(clippy::wildcard_imports)]
use super::*;

impl JavaClient {
    pub fn handle_player_ground(&self, player: &Player, ground: &SSetPlayerGround) {
        // A movement packet was received this tick — tracked for SClientTickEnd zeroing.
        self.received_movement_this_tick
            .store(true, Ordering::Relaxed);
        let entity = &player.living_entity.entity;
        if !entity.has_vehicle()
            && player
                .awaiting_teleport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
        {
            self.record_movement_packet();
        }
        player
            .living_entity
            .entity
            .on_ground
            .store(ground.on_ground, Ordering::Relaxed);
    }
}
