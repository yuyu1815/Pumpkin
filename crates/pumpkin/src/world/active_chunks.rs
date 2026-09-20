use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use pumpkin_util::math::vector2::Vector2;
use rustc_hash::{FxHashMap, FxHashSet};
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ActivePlayerArea {
    pub center: Vector2<i32>,
    pub simulation_distance: i32,
}

pub(crate) type ChunkSet = FxHashSet<Vector2<i32>>;

#[derive(Default)]
struct TrackerState {
    players: FxHashMap<Uuid, ActivePlayerArea>,
    loaded_active_chunks: ChunkSet,
    watcher_counts: FxHashMap<Vector2<i32>, u32>,
    forced_chunks: ChunkSet,
    tracked_forced_chunks: ChunkSet,
    tracked_active_chunks: ChunkSet,
}

/// Owns forced chunks, player tracking counts, loaded-active bookkeeping, and
/// the immutable active-chunk snapshot read by tickers and observers.
///
/// Mutations are serialized by `state`; readers use `snapshot` and therefore
/// keep a stable `Arc` even when the next tick publishes a new set.
pub(crate) struct ActiveChunkTracker {
    state: Mutex<TrackerState>,
    published_active_chunks: ArcSwap<ChunkSet>,
}

impl Default for ActiveChunkTracker {
    fn default() -> Self {
        Self {
            state: Mutex::new(TrackerState::default()),
            published_active_chunks: ArcSwap::from_pointee(ChunkSet::default()),
        }
    }
}

impl ActiveChunkTracker {
    fn add_chunk(
        state: &mut TrackerState,
        pos: Vector2<i32>,
        newly_active: &mut Vec<Vector2<i32>>,
    ) {
        let count = state.watcher_counts.entry(pos).or_default();
        *count += 1;
        if *count == 1 {
            state.tracked_active_chunks.insert(pos);
            newly_active.push(pos);
        }
    }

    fn remove_chunk(state: &mut TrackerState, pos: Vector2<i32>) {
        let Some(count) = state.watcher_counts.get_mut(&pos) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            state.watcher_counts.remove(&pos);
            state.loaded_active_chunks.remove(&pos);
            state.tracked_active_chunks.remove(&pos);
        }
    }

    fn add_area(
        state: &mut TrackerState,
        area: ActivePlayerArea,
        newly_active: &mut Vec<Vector2<i32>>,
    ) {
        let offsets = pumpkin_data::chunk_view_lut::get_chebyshev_square(
            area.simulation_distance.max(0) as u8,
        );
        for &(dx, dz) in offsets {
            Self::add_chunk(
                state,
                area.center.add_raw(dx as i32, dz as i32),
                newly_active,
            );
        }
    }

    fn remove_area(state: &mut TrackerState, area: ActivePlayerArea) {
        let offsets = pumpkin_data::chunk_view_lut::get_chebyshev_square(
            area.simulation_distance.max(0) as u8,
        );
        for &(dx, dz) in offsets {
            Self::remove_chunk(state, area.center.add_raw(dx as i32, dz as i32));
        }
    }

    fn update_player(
        state: &mut TrackerState,
        id: Uuid,
        area: ActivePlayerArea,
        newly_active: &mut Vec<Vector2<i32>>,
    ) {
        let previous = state.players.insert(id, area);
        match previous {
            None => Self::add_area(state, area, newly_active),
            Some(previous) if previous != area => {
                let prev_offsets = pumpkin_data::chunk_view_lut::get_chebyshev_square(
                    previous.simulation_distance.max(0) as u8,
                );
                for &(dx, dz) in prev_offsets {
                    let pos = previous.center.add_raw(dx as i32, dz as i32);
                    if (pos.x - area.center.x).abs() > area.simulation_distance
                        || (pos.y - area.center.y).abs() > area.simulation_distance
                    {
                        Self::remove_chunk(state, pos);
                    }
                }
                let curr_offsets = pumpkin_data::chunk_view_lut::get_chebyshev_square(
                    area.simulation_distance.max(0) as u8,
                );
                for &(dx, dz) in curr_offsets {
                    let pos = area.center.add_raw(dx as i32, dz as i32);
                    if (pos.x - previous.center.x).abs() > previous.simulation_distance
                        || (pos.y - previous.center.y).abs() > previous.simulation_distance
                    {
                        Self::add_chunk(state, pos, newly_active);
                    }
                }
            }
            Some(_) => {}
        }
    }

    fn remove_player(state: &mut TrackerState, id: Uuid) {
        if let Some(area) = state.players.remove(&id) {
            Self::remove_area(state, area);
        }
    }

    fn sync_forced_chunks(state: &mut TrackerState, newly_active: &mut Vec<Vector2<i32>>) {
        let removed: Vec<_> = state
            .tracked_forced_chunks
            .difference(&state.forced_chunks)
            .copied()
            .collect();
        let added: Vec<_> = state
            .forced_chunks
            .difference(&state.tracked_forced_chunks)
            .copied()
            .collect();
        for pos in removed {
            Self::remove_chunk(state, pos);
        }
        for pos in added {
            Self::add_chunk(state, pos, newly_active);
        }
        state.tracked_forced_chunks.clone_from(&state.forced_chunks);
    }

    fn publish(&self, state: &TrackerState) {
        self.published_active_chunks
            .store(Arc::new(state.tracked_active_chunks.clone()));
    }

    /// Reconciles the complete current player list, then applies forced chunks.
    /// The order matches the old World-owned flow: player updates, removals,
    /// forced chunks, then one published snapshot.
    pub(crate) fn update_players<I>(&self, players: I) -> Vec<Vector2<i32>>
    where
        I: IntoIterator<Item = (Uuid, ActivePlayerArea)>,
    {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut newly_active = Vec::new();
        let mut current_players = FxHashSet::default();

        for (id, area) in players {
            current_players.insert(id);
            Self::update_player(&mut state, id, area, &mut newly_active);
        }

        let removed_players: Vec<_> = state
            .players
            .keys()
            .filter(|id| !current_players.contains(id))
            .copied()
            .collect();
        for id in removed_players {
            Self::remove_player(&mut state, id);
        }
        Self::sync_forced_chunks(&mut state, &mut newly_active);
        self.publish(&state);
        newly_active
    }

    /// Returns a stable read snapshot. A later update does not mutate this `Arc`.
    pub(crate) fn snapshot(&self) -> Arc<ChunkSet> {
        self.published_active_chunks.load_full()
    }

    pub(crate) fn add_forced_chunk(&self, pos: Vector2<i32>) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .forced_chunks
            .insert(pos)
    }

    pub(crate) fn add_forced_chunks(&self, positions: impl IntoIterator<Item = Vector2<i32>>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .forced_chunks
            .extend(positions);
    }

    pub(crate) fn remove_forced_chunk(&self, pos: &Vector2<i32>) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .forced_chunks
            .remove(pos)
    }

    pub(crate) fn remove_forced_chunks(&self, positions: impl IntoIterator<Item = Vector2<i32>>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for pos in positions {
            state.forced_chunks.remove(&pos);
        }
    }

    pub(crate) fn clear_forced_chunks(&self) -> usize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = state.forced_chunks.len();
        state.forced_chunks.clear();
        count
    }

    pub(crate) fn is_forced(&self, pos: &Vector2<i32>) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .forced_chunks
            .contains(pos)
    }

    pub(crate) fn forced_chunks_snapshot(&self) -> ChunkSet {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .forced_chunks
            .clone()
    }

    pub(crate) fn mark_loaded_active(&self, pos: &Vector2<i32>) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.tracked_active_chunks.contains(pos) {
            return false;
        }
        state.loaded_active_chunks.insert(*pos)
    }

    pub(crate) fn mark_unloaded(&self, pos: &Vector2<i32>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .loaded_active_chunks
            .remove(pos);
    }

    pub(crate) fn loaded_active_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .loaded_active_chunks
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(x: i32, z: i32, simulation_distance: i32) -> ActivePlayerArea {
        ActivePlayerArea {
            center: Vector2::new(x, z),
            simulation_distance,
        }
    }

    #[test]
    fn follows_player_boundary_crossing() {
        let id = Uuid::from_u128(1);
        let tracker = ActiveChunkTracker::default();

        tracker.update_players([(id, area(0, 0, 1))]);
        assert_eq!(tracker.snapshot().len(), 9);

        let newly_active = tracker.update_players([(id, area(1, 0, 1))]);

        assert_eq!(tracker.snapshot().len(), 9);
        assert_eq!(newly_active.len(), 3);
        let active = tracker.snapshot();
        assert!(!active.contains(&Vector2::new(-1, 0)));
        assert!(active.contains(&Vector2::new(2, 0)));
    }

    #[test]
    fn removes_chunks_when_player_leaves() {
        let id = Uuid::from_u128(1);
        let tracker = ActiveChunkTracker::default();

        tracker.update_players([(id, area(0, 0, 1))]);
        tracker.update_players(std::iter::empty::<(Uuid, ActivePlayerArea)>());

        assert!(tracker.snapshot().is_empty());
        assert_eq!(tracker.loaded_active_count(), 0);
    }

    #[test]
    fn keeps_overlaps_active_until_both_players_leave() {
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let tracker = ActiveChunkTracker::default();

        tracker.update_players([(first, area(0, 0, 1)), (second, area(0, 0, 1))]);
        tracker.update_players([(second, area(0, 0, 1))]);

        assert_eq!(tracker.snapshot().len(), 9);

        tracker.update_players(std::iter::empty::<(Uuid, ActivePlayerArea)>());
        assert!(tracker.snapshot().is_empty());
    }

    #[test]
    fn forced_chunks_win_over_player_removal_until_released() {
        let id = Uuid::from_u128(1);
        let forced = Vector2::new(0, 0);
        let tracker = ActiveChunkTracker::default();

        tracker.update_players([(id, area(0, 0, 0))]);
        assert!(tracker.add_forced_chunk(forced));
        tracker.update_players(std::iter::empty::<(Uuid, ActivePlayerArea)>());
        assert!(tracker.snapshot().contains(&forced));

        assert!(tracker.remove_forced_chunk(&forced));
        tracker.update_players(std::iter::empty::<(Uuid, ActivePlayerArea)>());
        assert!(!tracker.snapshot().contains(&forced));
    }
}
