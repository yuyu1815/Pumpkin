use std::sync::Arc;

use arc_swap::{ArcSwap, Guard};

use crate::entity::player::Player;

/// Owns player-list writes while exposing only read snapshots to other modules.
pub(crate) struct PlayerMembership {
    players: ArcSwap<Vec<Arc<Player>>>,
}

impl Default for PlayerMembership {
    fn default() -> Self {
        Self {
            players: ArcSwap::new(Arc::new(Vec::new())),
        }
    }
}

impl PlayerMembership {
    pub(crate) fn load(&self) -> Guard<Arc<Vec<Arc<Player>>>> {
        self.players.load()
    }

    pub(crate) fn load_full(&self) -> Arc<Vec<Arc<Player>>> {
        self.players.load_full()
    }

    pub(super) fn publish(&self, player: &Arc<Player>) {
        self.players.rcu(|current_list| {
            let mut new_list = (**current_list).clone();
            new_list.push(player.clone());
            new_list
        });
    }

    pub(super) fn remove(&self, player: &Arc<Player>) -> Option<Arc<Player>> {
        let mut removed_player = None;
        self.players.rcu(|current_list| {
            let mut new_list = (**current_list).clone();
            if let Some(pos) = new_list
                .iter()
                .position(|candidate| Arc::ptr_eq(candidate, player))
            {
                removed_player = Some(new_list.remove(pos));
            }
            new_list
        });
        removed_player
    }
}
