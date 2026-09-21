use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use futures::future::join_all;
use pumpkin_util::math::vector2::Vector2;
use tokio::{
    join,
    sync::{OnceCell, RwLock, mpsc},
};
use tracing::{debug, error, trace};

use crate::{
    chunk::{ChunkReadingError, ChunkWritingError, io::Dirtiable},
    level::LevelFolder,
};

use super::{ChunkSerializer, FileIO, LoadedData, run_blocking, sync_path};

/// A simple implementation of the `ChunkSerializer` trait that loads and saves data
/// to disk using parallelism and a lazy-loading cache keyed by file path.
///
/// ### Concurrency model
///
/// * `file_locks` — one `Arc<RwLock<S>>` per on-disk file, created lazily.
///   All readers/writers for the same region file share this lock, so there
///   are never two concurrent writers for the same file.
/// * `watchers` — a ref-count per path.  While a path has active watchers the
///   serializer is **not** evicted from the cache and the file is **not**
///   flushed to disk (the caller owns the flush lifecycle).
///
/// A pending path is either `Buffered` (the serializer has changes which have
/// not reached the file yet) or `Published` (the file has the changes but has
/// not been explicitly synced). Only the former pins a serializer. The state
/// uses a synchronous mutex because it is metadata only; no guard is held over
/// an await.
///
/// ### Lock ordering (must never be violated to avoid deadlocks)
///
/// 1. snapshot pending state, then release it
/// 2. `file_locks`, then release it
/// 3. individual serializer `RwLock<S>`
///
/// `watchers` is acquired in its own critical section. No async lock is held
/// while waiting for another async lock.
#[derive(Clone, Copy)]
enum PendingState {
    /// The serializer contains a logical update which still needs publishing.
    Buffered(u64),
    /// The logical update reached the file; only an explicit OS sync remains.
    Published(u64),
}

impl PendingState {
    const fn generation(self) -> u64 {
        match self {
            Self::Buffered(generation) | Self::Published(generation) => generation,
        }
    }

    const fn is_buffered(self) -> bool {
        matches!(self, Self::Buffered(_))
    }
}

pub struct ChunkFileManager<S: ChunkSerializer<WriteBackend = PathBuf>> {
    file_locks: RwLock<BTreeMap<PathBuf, Arc<ChunkSerializerLazyLoader<S>>>>,
    watchers: RwLock<BTreeMap<PathBuf, usize>>,
    /// Region files changed since the last explicit disk sync.
    pending_sync: Mutex<BTreeMap<PathBuf, PendingState>>,
    cache_capacity: usize,
    chunk_config: S::ChunkConfig,
}

pub(crate) trait PathFromLevelFolder {
    fn file_path(folder: &LevelFolder, file_name: &str) -> PathBuf;
}

struct ChunkSerializerLazyLoader<S: ChunkSerializer<WriteBackend = PathBuf>> {
    path: PathBuf,
    /// Initialised at most once; subsequent calls reuse the same Arc.
    internal: OnceCell<Arc<RwLock<S>>>,
}

impl<S: ChunkSerializer<WriteBackend = PathBuf> + 'static> ChunkSerializerLazyLoader<S> {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            internal: OnceCell::new(),
        }
    }

    /// Returns `true` only when no outside caller still holds a clone of this
    /// loader *or* the inner serializer.
    ///
    /// # Safety requirement
    /// **Must be called while the write-lock on the parent `file_locks` map is
    /// held.**  That guarantees no new `Arc` clones can be issued while we
    /// inspect the strong counts.
    fn can_remove(loader: &Arc<Self>) -> bool {
        // The map itself holds 1 strong count; anything above that means an
        // active caller still has a handle.
        if Arc::strong_count(loader) > 1 {
            return false;
        }
        loader
            .internal
            .get()
            .is_none_or(|arc| Arc::strong_count(arc) == 1)
    }

    /// Returns the serializer, initialising it from disk on the first call.
    async fn get(&self) -> Result<Arc<RwLock<S>>, ChunkReadingError> {
        self.internal
            .get_or_try_init(|| async {
                let serializer = self.read_from_disk().await?;
                Ok(Arc::new(RwLock::new(serializer)))
            })
            .await
            .cloned()
    }

    async fn read_from_disk(&self) -> Result<S, ChunkReadingError> {
        trace!("Opening file from disk: {}", self.path.display());

        match tokio::fs::read(&self.path).await {
            Ok(bytes) => {
                if bytes.is_empty() {
                    trace!(
                        "File is empty (0 bytes), using default for: {}",
                        self.path.display()
                    );
                    return Ok(S::default());
                }
                let value = run_blocking(move || S::read(bytes.into()))
                    .await
                    .map_err(|_| {
                        ChunkReadingError::IoError(std::io::Error::other(
                            "chunk deserialization task failed",
                        ))
                    })??;
                trace!("Successfully read file from disk: {}", self.path.display());
                Ok(value)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                trace!("File not found, using default for: {}", self.path.display());
                Ok(S::default())
            }
            Err(err) => Err(ChunkReadingError::IoError(err)),
        }
    }
}

impl<S: ChunkSerializer<WriteBackend = PathBuf>> ChunkFileManager<S> {
    const DEFAULT_CACHE_CAPACITY: usize = 64;

    pub fn new(chunk_config: S::ChunkConfig) -> Self {
        Self::with_capacity(chunk_config, Self::DEFAULT_CACHE_CAPACITY)
    }

    fn with_capacity(chunk_config: S::ChunkConfig, cache_capacity: usize) -> Self {
        Self {
            file_locks: RwLock::new(BTreeMap::new()),
            watchers: RwLock::new(BTreeMap::new()),
            pending_sync: Mutex::new(BTreeMap::new()),
            cache_capacity: cache_capacity.max(1),
            chunk_config,
        }
    }

    fn pending_state(&self, path: &Path) -> Option<PendingState> {
        self.pending_sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .copied()
    }

    fn mark_buffered(&self, path: &Path) -> u64 {
        let mut pending = self
            .pending_sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = pending
            .get(path)
            .map_or(0, |state| state.generation())
            .saturating_add(1);
        pending.insert(path.to_path_buf(), PendingState::Buffered(generation));
        generation
    }

    fn mark_published(&self, path: &Path, generation: u64) {
        let mut pending = self
            .pending_sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending
            .get(path)
            .is_some_and(|state| state.generation() == generation)
        {
            pending.insert(path.to_path_buf(), PendingState::Published(generation));
        }
    }

    fn clear_pending(&self, path: &Path, generation: u64) {
        let mut pending = self
            .pending_sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending
            .get(path)
            .is_some_and(|state| state.generation() == generation)
        {
            pending.remove(path);
        }
    }

    fn pending_snapshot(&self) -> Vec<(PathBuf, PendingState)> {
        self.pending_sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(path, state)| (path.clone(), *state))
            .collect()
    }
}

impl<S: ChunkSerializer<WriteBackend = PathBuf>> ChunkFileManager<S> {
    /// Returns the serializer for `path`, inserting a lazy-loader if absent.
    ///
    /// Uses an optimistic read-first pattern: in the common case (cache hit)
    /// we never need a write-lock on the map.
    async fn get_serializer(&self, path: &Path) -> Result<Arc<RwLock<S>>, ChunkReadingError> {
        {
            let locks = self.file_locks.read().await;
            if let Some(loader) = locks.get(path) {
                // Clone the Arc *before* releasing the lock so it stays alive.
                let loader = loader.clone();
                drop(locks);
                return loader.get().await;
            }
        }

        let loader = {
            let mut locks = self.file_locks.write().await;
            locks.get(path).cloned().unwrap_or_else(|| {
                if locks.len() >= self.cache_capacity {
                    let evictable = locks.iter().find_map(|(cached_path, loader)| {
                        let watched = self.watchers.try_read().map_or(true, |watchers| {
                            watchers.get(cached_path).is_some_and(|&count| count > 0)
                        });
                        let buffered = self
                            .pending_state(cached_path)
                            .is_some_and(PendingState::is_buffered);
                        (!watched && !buffered && ChunkSerializerLazyLoader::can_remove(loader))
                            .then(|| cached_path.clone())
                    });
                    if let Some(evictable) = evictable {
                        locks.remove(&evictable);
                    } else {
                        // The limit is an eviction target, not a correctness limit:
                        // active regions may temporarily exceed it during one batch.
                        trace!(
                            "Serializer cache temporarily exceeds capacity {} while opening {}",
                            self.cache_capacity,
                            path.display()
                        );
                    }
                }

                let loader = Arc::new(ChunkSerializerLazyLoader::new(path.into()));
                locks.insert(path.into(), loader.clone());
                loader
            })
            // Write-lock dropped here — `loader.get()` may block on I/O and
            // must not hold the map lock.
        };

        loader.get().await
    }

    /// Attempt to evict the cached serializer for `path`.
    ///
    /// The entry is only removed when *both* conditions hold:
    /// 1. No watcher still references the path.
    /// 2. No other `Arc` clone is live (ensured via `can_remove`).
    async fn maybe_evict(&self, path: &PathBuf) {
        // Check watchers independently of file_locks to honour lock ordering.
        let still_watched = {
            let watchers = self.watchers.read().await;
            watchers.get(path).is_some_and(|&c| c > 0)
        };

        if still_watched {
            return;
        }

        if self
            .pending_state(path)
            .is_some_and(PendingState::is_buffered)
        {
            trace!(
                "Skipping eviction for {} — serializer has unpublished data",
                path.display()
            );
            return;
        }

        let mut locks = self.file_locks.write().await;
        let removable = locks
            .get(path)
            .is_some_and(ChunkSerializerLazyLoader::can_remove);

        if removable {
            locks.remove(path);
            trace!("Evicted serializer cache for {}", path.display());
        } else {
            trace!(
                "Skipping eviction for {} — references still live",
                path.display()
            );
        }
    }
}

impl<P, S> FileIO for ChunkFileManager<S>
where
    P: PathFromLevelFolder + Send + Sync + Sized + Dirtiable + 'static,
    S: ChunkSerializer<Data = P, WriteBackend = PathBuf>,
    S::ChunkConfig: Send + Sync,
{
    type Data = Arc<S::Data>;

    async fn watch_chunks<'a>(&'a self, folder: &'a LevelFolder, chunks: &'a [Vector2<i32>]) {
        let paths: Vec<_> = chunks
            .iter()
            .map(|c| P::file_path(folder, &S::get_chunk_key(c)))
            .collect();

        let mut watchers = self.watchers.write().await;
        for path in paths {
            *watchers.entry(path).or_insert(0) += 1;
        }
    }

    async fn unwatch_chunks<'a>(&'a self, folder: &'a LevelFolder, chunks: &'a [Vector2<i32>]) {
        let paths: Vec<_> = chunks
            .iter()
            .map(|c| P::file_path(folder, &S::get_chunk_key(c)))
            .collect();

        let mut paths_to_evict = Vec::new();
        {
            let mut watchers = self.watchers.write().await;
            for path in paths {
                if let std::collections::btree_map::Entry::Occupied(mut e) = watchers.entry(path) {
                    let count = e.get_mut();
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        let (path, _) = e.remove_entry();
                        paths_to_evict.push(path);
                    }
                }
            }
        }

        for path in paths_to_evict {
            self.maybe_evict(&path).await;
        }
    }

    async fn clear_watched_chunks(&self) {
        let paths: Vec<PathBuf> = {
            let mut watchers = self.watchers.write().await;
            let keys: Vec<_> = watchers.keys().cloned().collect();
            watchers.clear();
            keys
        };
        for path in paths {
            self.maybe_evict(&path).await;
        }
    }

    async fn fetch_chunks<'a>(
        &'a self,
        folder: &'a LevelFolder,
        chunk_coords: &'a [Vector2<i32>],
        stream: mpsc::Sender<LoadedData<Self::Data, ChunkReadingError>>,
    ) {
        // Group requested chunk coords by their region file.
        let mut regions_chunks: BTreeMap<String, Vec<Vector2<i32>>> = BTreeMap::new();
        for at in chunk_coords {
            regions_chunks
                .entry(S::get_chunk_key(at))
                .or_default()
                .push(*at);
        }

        let region_tasks = regions_chunks.into_iter().map(|(file_name, chunks)| {
            let task_stream = stream.clone();
            async move {
                let path = P::file_path(folder, &file_name);

                let chunk_serializer = match self.get_serializer(&path).await {
                    Ok(s) => s,
                    Err(ChunkReadingError::ChunkNotExist) => {
                        // Every requested coordinate needs a terminal result. An absent
                        // region is normal for a new world and means Missing, not a
                        // closed channel that the caller could mistake for Missing.
                        for pos in chunks {
                            if task_stream.send(LoadedData::Missing(pos)).await.is_err() {
                                return;
                            }
                        }
                        return;
                    }
                    Err(err) => {
                        // Report the region-open failure for every requested coordinate.
                        // The caller may use a batch; one error for only chunks[0] would
                        // leave the remaining waiters without a terminal outcome.
                        let message = err.to_string();
                        for pos in chunks {
                            let error =
                                ChunkReadingError::IoError(std::io::Error::other(message.clone()));
                            if task_stream
                                .send(LoadedData::Error((pos, error)))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        return;
                    }
                };

                // A bounded channel of 1 keeps backpressure between the
                // serializer and the caller without unbounded buffering.
                let (send, mut recv) = mpsc::channel::<LoadedData<S::Data, ChunkReadingError>>(1);

                // Forward received chunks, wrapping them in `Arc`.
                // Captured move is intentional — `task_stream` is consumed here.
                let forward = async move {
                    while let Some(data) = recv.recv().await {
                        let wrapped = data.map_loaded(Arc::new);
                        if task_stream.send(wrapped).await.is_err() {
                            // Receiver dropped; abort early to avoid wasted work.
                            return;
                        }
                    }
                };

                // Hold the read lock only for the duration of `get_chunks`.
                let read = async move {
                    let serializer = chunk_serializer.read().await;
                    serializer.get_chunks(chunks, send).await;
                };

                join!(forward, read);

                // Evict if not watched and references are dropped
                self.maybe_evict(&path).await;
            }
        });

        join_all(region_tasks).await;
    }

    async fn save_chunks<'a>(
        &'a self,
        folder: &'a LevelFolder,
        chunks_data: Vec<(Vector2<i32>, Self::Data)>,
    ) -> Result<(), ChunkWritingError> {
        // Group chunks by region file.
        let mut regions_chunks: BTreeMap<String, Vec<Self::Data>> = BTreeMap::new();
        for (at, chunk) in chunks_data {
            regions_chunks
                .entry(S::get_chunk_key(&at))
                .or_default()
                .push(chunk);
        }

        let tasks = regions_chunks
            .into_iter()
            .map(|(file_name, chunk_locks)| async move {
                let path = P::file_path(folder, &file_name);
                trace!("Saving chunks into {}", path.display());

                let chunk_serializer = match self.get_serializer(&path).await {
                    Ok(s) => s,
                    Err(ChunkReadingError::ChunkNotExist) => {
                        return Err(ChunkWritingError::IoError(std::io::Error::other(
                            "get_serializer returned ChunkNotExist",
                        )));
                    }
                    Err(ChunkReadingError::IoError(err)) => {
                        error!("I/O error reading region before write: {err}");
                        return Err(ChunkWritingError::IoError(err));
                    }
                    Err(err) => {
                        return Err(ChunkWritingError::IoError(std::io::Error::other(
                            err.to_string(),
                        )));
                    }
                };

                let mut cleared_dirty = Vec::new();
                let mut pending_generation = None;
                let mut update_error = None;
                {
                    let mut writer = chunk_serializer.write().await;
                    for chunk in &chunk_locks {
                        // Atomically snapshot and clear the dirty flag before we
                        // write so that any mutation that races in *during* this
                        // serialisation round will mark dirty again correctly.
                        let was_dirty = chunk.is_dirty();
                        chunk.mark_dirty(false);

                        if was_dirty {
                            cleared_dirty.push(chunk.clone());
                            if let Err(error) =
                                writer.update_chunk(chunk.clone(), &self.chunk_config).await
                            {
                                for chunk in &cleared_dirty {
                                    chunk.mark_dirty(true);
                                }
                                // Keep the loader pinned even if an
                                // implementation fails after partially changing
                                // its in-memory state.
                                pending_generation = Some(self.mark_buffered(&path));
                                update_error = Some(error);
                                break;
                            }
                        }
                    }
                    if update_error.is_none() && !cleared_dirty.is_empty() {
                        pending_generation = Some(self.mark_buffered(&path));
                    }
                }

                if let Some(error) = update_error {
                    drop(chunk_serializer);
                    self.maybe_evict(&path).await;
                    return Err(error);
                }

                trace!("Chunk data updated for {}", path.display());

                // We check watchers after releasing the serializer lock. A
                // buffered serializer is pinned; a published one may evict.
                let is_watched = {
                    let watchers = self.watchers.read().await;
                    watchers.get(&path).is_some_and(|&c| c > 0)
                };

                if !is_watched {
                    if let Some(generation) = pending_generation {
                        let write_result = {
                            let serializer = chunk_serializer.write().await;
                            debug!("Flushing {} to disk", path.display());
                            let result = serializer.write(&path).await;
                            if result.is_ok() {
                                // Keep the serializer lock through this state
                                // transition so a newer update cannot be lost.
                                self.mark_published(&path, generation);
                            }
                            result
                        };
                        if let Err(error) = write_result {
                            for chunk in &cleared_dirty {
                                chunk.mark_dirty(true);
                            }
                            drop(chunk_serializer);
                            self.maybe_evict(&path).await;
                            return Err(ChunkWritingError::IoError(error));
                        }
                    }

                    drop(chunk_serializer);
                    self.maybe_evict(&path).await;
                }

                Ok(())
            });

        // Collect all region results; surface the first error encountered.
        let results: Vec<Result<(), ChunkWritingError>> = join_all(tasks).await;
        results.into_iter().find(Result::is_err).unwrap_or(Ok(()))
    }

    async fn sync_all<'a>(&'a self, _folder: &'a LevelFolder) -> Result<(), ChunkWritingError> {
        // Snapshot only metadata. Every async lock is acquired after this
        // synchronous guard is released, so eviction and sync have one order.
        let paths = self.pending_snapshot();

        for (path, _) in paths {
            let Some(state) = self.pending_state(&path) else {
                continue;
            };
            let loader = {
                let locks = self.file_locks.read().await;
                locks.get(&path).cloned()
            };
            let serializer = loader
                .as_ref()
                .and_then(|loader| loader.internal.get().cloned());

            match serializer {
                Some(serializer) => {
                    let result = {
                        let serializer = serializer.write().await;
                        let Some(state) = self.pending_state(&path) else {
                            continue;
                        };
                        let generation = state.generation();
                        let result = serializer.sync_all(&path).await;
                        if result.is_ok() {
                            // The serializer lock prevents a newer update from
                            // being hidden by this clear.
                            self.clear_pending(&path, generation);
                        }
                        result
                    };
                    result.map_err(ChunkWritingError::IoError)?;
                }
                None if state.is_buffered() => {
                    return Err(ChunkWritingError::IoError(std::io::Error::other(format!(
                        "pending serializer was evicted before publishing {}",
                        path.display()
                    ))));
                }
                None => {
                    // Published content is safe to sync by path after eviction.
                    // A missing file is an error, never a successful old-file sync.
                    sync_path(&path).await.map_err(ChunkWritingError::IoError)?;
                    self.clear_pending(&path, state.generation());
                }
            }

            self.maybe_evict(&path).await;
        }

        Ok(())
    }

    /// Blocks until all in-flight serialiser operations have completed by
    /// acquiring (and immediately releasing) a write-lock on every cached
    /// serialiser.
    ///
    /// This is a linearisation point: after this future resolves no mutation
    /// started before the call is still running.
    async fn block_and_await_ongoing_tasks(&self) {
        // Snapshot the current set of loaders under a read-lock so we do
        // not block new insertions longer than necessary.
        let loaders: Vec<Arc<ChunkSerializerLazyLoader<S>>> =
            { self.file_locks.read().await.values().cloned().collect() };

        // For each loader that has been initialised, acquire a write-lock
        // and release it immediately.  This guarantees that any concurrent
        // read or write operation that was in progress has finished.
        let drain_tasks = loaders.into_iter().map(|loader| async move {
            if let Some(serializer_arc) = loader.internal.get() {
                // Acquiring + immediately dropping the write-lock acts as a
                // barrier: it can only succeed once all current lock holders
                // have released their guards.
                let _guard = serializer_arc.write().await;
            }
        });

        join_all(drain_tasks).await;
    }
}

pub enum LevelFileIO<Linear, Anvil, Pump>
where
    Linear: ChunkSerializer<WriteBackend = PathBuf>,
    Anvil: ChunkSerializer<WriteBackend = PathBuf>,
    Pump: ChunkSerializer<WriteBackend = PathBuf>,
{
    Linear(ChunkFileManager<Linear>),
    Anvil(ChunkFileManager<Anvil>),
    Pump(ChunkFileManager<Pump>),
}

impl<P, Linear, Anvil, Pump> FileIO for LevelFileIO<Linear, Anvil, Pump>
where
    P: PathFromLevelFolder + Send + Sync + Sized + Dirtiable + 'static,
    Linear: ChunkSerializer<Data = P, WriteBackend = PathBuf>,
    Anvil: ChunkSerializer<Data = P, WriteBackend = PathBuf>,
    Pump: ChunkSerializer<Data = P, WriteBackend = PathBuf>,
    Linear::ChunkConfig: Send + Sync,
    Anvil::ChunkConfig: Send + Sync,
    Pump::ChunkConfig: Send + Sync,
{
    type Data = Arc<P>;

    async fn fetch_chunks<'a>(
        &'a self,
        folder: &'a LevelFolder,
        chunk_coords: &'a [Vector2<i32>],
        stream: tokio::sync::mpsc::Sender<LoadedData<Self::Data, ChunkReadingError>>,
    ) {
        match self {
            Self::Linear(io) => io.fetch_chunks(folder, chunk_coords, stream).await,
            Self::Anvil(io) => io.fetch_chunks(folder, chunk_coords, stream).await,
            Self::Pump(io) => io.fetch_chunks(folder, chunk_coords, stream).await,
        }
    }

    async fn save_chunks<'a>(
        &'a self,
        folder: &'a LevelFolder,
        chunks_data: Vec<(Vector2<i32>, Self::Data)>,
    ) -> Result<(), ChunkWritingError> {
        match self {
            Self::Linear(io) => io.save_chunks(folder, chunks_data).await,
            Self::Anvil(io) => io.save_chunks(folder, chunks_data).await,
            Self::Pump(io) => io.save_chunks(folder, chunks_data).await,
        }
    }

    async fn watch_chunks<'a>(&'a self, folder: &'a LevelFolder, chunks: &'a [Vector2<i32>]) {
        match self {
            Self::Linear(io) => io.watch_chunks(folder, chunks).await,
            Self::Anvil(io) => io.watch_chunks(folder, chunks).await,
            Self::Pump(io) => io.watch_chunks(folder, chunks).await,
        }
    }

    async fn unwatch_chunks<'a>(&'a self, folder: &'a LevelFolder, chunks: &'a [Vector2<i32>]) {
        match self {
            Self::Linear(io) => io.unwatch_chunks(folder, chunks).await,
            Self::Anvil(io) => io.unwatch_chunks(folder, chunks).await,
            Self::Pump(io) => io.unwatch_chunks(folder, chunks).await,
        }
    }

    async fn clear_watched_chunks(&self) {
        match self {
            Self::Linear(io) => io.clear_watched_chunks().await,
            Self::Anvil(io) => io.clear_watched_chunks().await,
            Self::Pump(io) => io.clear_watched_chunks().await,
        }
    }

    async fn block_and_await_ongoing_tasks(&self) {
        match self {
            Self::Linear(io) => io.block_and_await_ongoing_tasks().await,
            Self::Anvil(io) => io.block_and_await_ongoing_tasks().await,
            Self::Pump(io) => io.block_and_await_ongoing_tasks().await,
        }
    }

    async fn sync_all<'a>(&'a self, folder: &'a LevelFolder) -> Result<(), ChunkWritingError> {
        match self {
            Self::Linear(io) => io.sync_all(folder).await,
            Self::Anvil(io) => io.sync_all(folder).await,
            Self::Pump(io) => io.sync_all(folder).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use bytes::Bytes;
    use pumpkin_config::chunk::AnvilChunkConfig;
    use pumpkin_data::Block;
    use tempfile::TempDir;

    use super::ChunkFileManager;
    use crate::chunk::format::anvil::{AnvilChunkFile, SingleChunkDataSerializer};
    use crate::chunk::io::{Dirtiable, FileIO, LoadedData};
    use crate::chunk::{ChunkData, ChunkReadingError, ChunkSerializingError};
    use crate::level::LevelFolder;
    use pumpkin_util::math::vector2::Vector2;

    struct FailingChunk {
        dirty: std::sync::atomic::AtomicBool,
    }

    impl Dirtiable for FailingChunk {
        fn is_dirty(&self) -> bool {
            self.dirty.load(std::sync::atomic::Ordering::Relaxed)
        }

        fn mark_dirty(&self, flag: bool) {
            self.dirty.store(flag, std::sync::atomic::Ordering::Relaxed);
        }
    }

    impl super::PathFromLevelFolder for FailingChunk {
        fn file_path(folder: &LevelFolder, file_name: &str) -> std::path::PathBuf {
            folder.region_folder.join(file_name)
        }
    }

    impl SingleChunkDataSerializer for FailingChunk {
        fn to_bytes(&self) -> Result<Bytes, ChunkSerializingError> {
            Err(ChunkSerializingError::ErrorSerializingChunk(
                pumpkin_nbt::Error::UnsupportedType("test failure".into()),
            ))
        }

        fn from_bytes(_bytes: &Bytes, _pos: Vector2<i32>) -> Result<Self, ChunkReadingError> {
            Err(ChunkReadingError::ChunkNotExist)
        }

        fn position(&self) -> (i32, i32) {
            (0, 0)
        }
    }

    #[tokio::test]
    async fn watched_region_is_published_by_flush_after_unwatch() {
        let temp_dir = TempDir::new().expect("temp directory");
        let level_folder = LevelFolder {
            root_folder: temp_dir.path().to_path_buf(),
            dim_folder: temp_dir.path().to_path_buf(),
            region_folder: temp_dir.path().join("region"),
            entities_folder: temp_dir.path().join("entities"),
            poi_folder: temp_dir.path().join("poi"),
        };
        fs::create_dir_all(&level_folder.region_folder).expect("region directory");
        let position = Vector2::new(0, 0);
        let saver = ChunkFileManager::<AnvilChunkFile<ChunkData>>::new(AnvilChunkConfig::default());

        let old = Arc::new(ChunkData::empty(0, 0));
        old.mark_dirty(true);
        saver
            .save_chunks(&level_folder, vec![(position, old)])
            .await
            .expect("initial region write");

        let updated = Arc::new(ChunkData::empty(0, 0));
        updated.set_block_absolute_y(0, 64, 0, Block::STONE.default_state.id);
        updated.mark_dirty(true);
        saver.watch_chunks(&level_folder, &[position]).await;
        saver
            .save_chunks(&level_folder, vec![(position, updated)])
            .await
            .expect("watched logical update");
        saver.unwatch_chunks(&level_folder, &[position]).await;
        saver
            .sync_all(&level_folder)
            .await
            .expect("flush publishes watched update");

        let retry_saver =
            ChunkFileManager::<AnvilChunkFile<ChunkData>>::new(AnvilChunkConfig::default());
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        retry_saver
            .fetch_chunks(&level_folder, &[position], sender)
            .await;
        let LoadedData::Loaded(loaded) = receiver.recv().await.expect("terminal load result")
        else {
            panic!("flushed watched update must reload");
        };
        assert_eq!(
            loaded.section.get_block_absolute_y(0, 64, 0),
            Some(Block::STONE.default_state.id),
            "unwatch must not evict an unpublished pending serializer"
        );
    }

    #[tokio::test]
    async fn normal_saves_do_not_pin_cache_without_explicit_flush() {
        let temp_dir = TempDir::new().expect("temp directory");
        let level_folder = LevelFolder {
            root_folder: temp_dir.path().to_path_buf(),
            dim_folder: temp_dir.path().to_path_buf(),
            region_folder: temp_dir.path().join("region"),
            entities_folder: temp_dir.path().join("entities"),
            poi_folder: temp_dir.path().join("poi"),
        };
        fs::create_dir_all(&level_folder.region_folder).expect("region directory");
        let saver = ChunkFileManager::<AnvilChunkFile<ChunkData>>::with_capacity(
            AnvilChunkConfig::default(),
            1,
        );

        for position in [Vector2::new(0, 0), Vector2::new(32, 0), Vector2::new(64, 0)] {
            let chunk = Arc::new(ChunkData::empty(position.x, position.y));
            chunk.mark_dirty(true);
            saver
                .save_chunks(&level_folder, vec![(position, chunk)])
                .await
                .expect("normal region write");
            assert!(
                saver.file_locks.read().await.len() <= 1,
                "normal nonflush saves must not pin every region serializer"
            );
        }
        saver
            .sync_all(&level_folder)
            .await
            .expect("explicit flush must sync evicted normal writes");
        assert!(
            saver
                .pending_state(&level_folder.region_folder.join("r.0.0.mca"))
                .is_none()
        );
        assert!(
            saver
                .pending_state(&level_folder.region_folder.join("r.1.0.mca"))
                .is_none()
        );
        assert!(
            saver
                .pending_state(&level_folder.region_folder.join("r.2.0.mca"))
                .is_none()
        );
    }

    #[tokio::test]
    async fn oversized_plain_region_batch_is_not_rejected_by_cache_target() {
        let temp_dir = TempDir::new().expect("temp directory");
        let level_folder = LevelFolder {
            root_folder: temp_dir.path().to_path_buf(),
            dim_folder: temp_dir.path().to_path_buf(),
            region_folder: temp_dir.path().join("region"),
            entities_folder: temp_dir.path().join("entities"),
            poi_folder: temp_dir.path().join("poi"),
        };
        fs::create_dir_all(&level_folder.region_folder).expect("region directory");
        let saver = ChunkFileManager::<AnvilChunkFile<ChunkData>>::new(AnvilChunkConfig::default());
        let chunks = (0..=64)
            .map(|region| {
                let position = Vector2::new(region * 32, 0);
                let chunk = Arc::new(ChunkData::empty(position.x, position.y));
                chunk.mark_dirty(true);
                (position, chunk)
            })
            .collect();

        saver
            .save_chunks(&level_folder, chunks)
            .await
            .expect("normal writes beyond the cache target must remain valid");
        saver
            .sync_all(&level_folder)
            .await
            .expect("explicit flush after the oversized normal batch");
        assert!(saver.pending_sync.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sync_and_eviction_contention_has_a_bounded_completion() {
        let temp_dir = TempDir::new().expect("temp directory");
        let level_folder = LevelFolder {
            root_folder: temp_dir.path().to_path_buf(),
            dim_folder: temp_dir.path().to_path_buf(),
            region_folder: temp_dir.path().join("region"),
            entities_folder: temp_dir.path().join("entities"),
            poi_folder: temp_dir.path().join("poi"),
        };
        fs::create_dir_all(&level_folder.region_folder).expect("region directory");
        let position = Vector2::new(0, 0);
        let saver = ChunkFileManager::<AnvilChunkFile<ChunkData>>::with_capacity(
            AnvilChunkConfig::default(),
            1,
        );

        for _ in 0..8 {
            saver.watch_chunks(&level_folder, &[position]).await;
            let chunk = Arc::new(ChunkData::empty(position.x, position.y));
            chunk.mark_dirty(true);
            saver
                .save_chunks(&level_folder, vec![(position, chunk)])
                .await
                .expect("watched logical update");

            let watched_chunks = [position];
            let result = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                let (sync_result, ()) = tokio::join!(
                    saver.sync_all(&level_folder),
                    saver.unwatch_chunks(&level_folder, &watched_chunks),
                );
                sync_result
            })
            .await
            .expect("sync and eviction must not deadlock");
            result.expect("contention sync");
        }
    }

    #[tokio::test]
    async fn failed_sync_keeps_pending_state_for_retry_and_then_evicts() {
        let temp_dir = TempDir::new().expect("temp directory");
        let level_folder = LevelFolder {
            root_folder: temp_dir.path().to_path_buf(),
            dim_folder: temp_dir.path().to_path_buf(),
            region_folder: temp_dir.path().join("region"),
            entities_folder: temp_dir.path().join("entities"),
            poi_folder: temp_dir.path().join("poi"),
        };
        fs::create_dir_all(&level_folder.region_folder).expect("region directory");
        let position = Vector2::new(0, 0);
        let saver = ChunkFileManager::<AnvilChunkFile<ChunkData>>::new(AnvilChunkConfig::default());
        let initial = Arc::new(ChunkData::empty(0, 0));
        initial.mark_dirty(true);
        saver
            .save_chunks(&level_folder, vec![(position, initial)])
            .await
            .expect("initial region write");
        let path = level_folder.region_folder.join("r.0.0.mca");
        fs::remove_file(&path).expect("remove region for forced sync failure");

        assert!(saver.sync_all(&level_folder).await.is_err());
        assert!(saver.pending_state(&path).is_some());

        let retry = Arc::new(ChunkData::empty(0, 0));
        retry.set_block_absolute_y(0, 64, 0, Block::STONE.default_state.id);
        retry.mark_dirty(true);
        saver
            .save_chunks(&level_folder, vec![(position, retry)])
            .await
            .expect("logical retry after sync failure");
        saver.sync_all(&level_folder).await.expect("retry flush");
        assert!(saver.pending_state(&path).is_none());
        assert!(saver.file_locks.read().await.get(&path).is_none());
    }

    #[tokio::test]
    async fn failed_update_keeps_chunk_dirty_for_retry() {
        let temp_dir = TempDir::new().expect("temp directory");
        let level_folder = LevelFolder {
            root_folder: temp_dir.path().to_path_buf(),
            dim_folder: temp_dir.path().to_path_buf(),
            region_folder: temp_dir.path().join("region"),
            entities_folder: temp_dir.path().join("entities"),
            poi_folder: temp_dir.path().join("poi"),
        };
        fs::create_dir_all(&level_folder.region_folder).expect("region directory");

        let chunk = Arc::new(FailingChunk {
            dirty: std::sync::atomic::AtomicBool::new(true),
        });
        let saver =
            ChunkFileManager::<AnvilChunkFile<FailingChunk>>::new(AnvilChunkConfig::default());
        let error = saver
            .save_chunks(&level_folder, vec![(Vector2::new(0, 0), chunk.clone())])
            .await
            .expect_err("update failure must surface");

        assert!(matches!(
            error,
            crate::chunk::ChunkWritingError::ChunkSerializingError(_)
        ));
        assert!(chunk.is_dirty(), "failed update must remain retryable");
        assert!(
            saver
                .pending_state(&level_folder.region_folder.join("r.0.0.mca"))
                .is_some_and(super::PendingState::is_buffered),
            "failed update must protect possibly buffered serializer state"
        );
        assert!(!level_folder.region_folder.join("r.0.0.mca").exists());
    }

    #[tokio::test]
    async fn failed_region_write_keeps_chunk_dirty_for_retry() {
        let temp_dir = TempDir::new().expect("temp directory");
        let level_folder = LevelFolder {
            root_folder: temp_dir.path().to_path_buf(),
            dim_folder: temp_dir.path().to_path_buf(),
            region_folder: temp_dir.path().join("region"),
            entities_folder: temp_dir.path().join("entities"),
            poi_folder: temp_dir.path().join("poi"),
        };
        fs::create_dir_all(&level_folder.region_folder).expect("region directory");

        let position = Vector2::new(0, 0);
        let chunk = Arc::new(ChunkData::empty(0, 0));
        chunk.mark_dirty(true);
        let saver = ChunkFileManager::<AnvilChunkFile<ChunkData>>::new(AnvilChunkConfig::default());

        saver
            .save_chunks(&level_folder, vec![(position, chunk.clone())])
            .await
            .expect("initial region write");
        let region_path = level_folder.region_folder.join("r.0.0.mca");
        let original = fs::read(&region_path).expect("initial region bytes");

        chunk.mark_dirty(true);
        let temp_path = region_path.with_extension("tmp");
        fs::create_dir(&temp_path).expect("blocking temp path");
        let error = saver
            .save_chunks(&level_folder, vec![(position, chunk.clone())])
            .await
            .expect_err("blocked temp path must fail the write");

        assert!(matches!(error, crate::chunk::ChunkWritingError::IoError(_)));
        assert!(
            saver
                .pending_state(&region_path)
                .is_some_and(super::PendingState::is_buffered),
            "failed logical write must keep the serializer protected"
        );
        assert!(
            saver.sync_all(&level_folder).await.is_err(),
            "syncing the old disk file must not report a failed buffered write as success"
        );
        assert!(chunk.is_dirty(), "failed write must remain retryable");
        assert_eq!(fs::read(&region_path).expect("region bytes"), original);

        fs::remove_dir(&temp_path).expect("remove blocking temp path");
        saver
            .save_chunks(&level_folder, vec![(position, chunk)])
            .await
            .expect("retry region write");

        let retry_saver =
            ChunkFileManager::<AnvilChunkFile<ChunkData>>::new(AnvilChunkConfig::default());
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        retry_saver
            .fetch_chunks(&level_folder, &[position], sender)
            .await;
        let loaded = receiver.recv().await.expect("terminal load result");
        let LoadedData::Loaded(loaded) = loaded else {
            panic!("retry must reload the saved chunk");
        };
        assert_eq!((loaded.x, loaded.z), (0, 0));
    }
}
