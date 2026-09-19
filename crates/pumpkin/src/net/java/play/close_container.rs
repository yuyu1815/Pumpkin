#[allow(clippy::wildcard_imports)]
use super::*;

impl JavaClient {
    pub fn handle_close_container(&self, player: &Arc<Player>, window_id: i32) {
        let current_handler = player
            .current_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let current_window_id = current_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sync_id();

        // A close packet is authoritative only for the handler it names. In
        // particular, do not let a delayed close for window A close window B
        // and return B's cursor/grid contents.
        if !window_id_matches(window_id, current_window_id) {
            return;
        }
        player.on_handled_screen_closed();
    }
}

fn window_id_matches(packet_window_id: i32, current_window_id: u8) -> bool {
    packet_window_id == i32::from(current_window_id)
}

#[cfg(test)]
mod tests {
    use super::window_id_matches;

    #[test]
    fn close_only_matches_the_current_window_id() {
        assert!(window_id_matches(0, 0));
        assert!(window_id_matches(42, 42));
        assert!(!window_id_matches(41, 42));
        assert!(!window_id_matches(-1, 0));
    }
}
