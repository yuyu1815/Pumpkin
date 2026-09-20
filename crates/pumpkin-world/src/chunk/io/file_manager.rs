use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
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

use super::{ChunkSerializer, FileIO, LoadedData, run_blocking};

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
/// ### Lock ordering (must never be violated to avoid deadlocks)
///
/// 1. `file_locks`  (outer)
/// 2. individual `RwLock<S>` inside each loader  (inner)
/// 3. `watchers`  (independent — never held at the same time as either above)
///
/// `watchers` is always acquired in its own critical section, after all
/// serializer locks are released, which keeps it strictly independent.
pub struct ChunkFileManager<S: ChunkSerializer<WriteBackend = PathBuf>> {
    file_locks: RwLock<BTreeMap<PathBuf, Arc<ChunkSerializerLazyLoader<S>>>>,
    watchers: RwLock<BTreeMap<PathBuf, usize>>,
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
    pub fn new(chunk_config: S::ChunkConfig) -> Self {
        Self {
            file_locks: RwLock::new(BTreeMap::new()),
            watchers: RwLock::new(BTreeMap::new()),
            chunk_config,
        }
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
            locks
                .entry(path.into())
                .or_insert_with(|| Arc::new(ChunkSerializerLazyLoader::new(path.into())))
                .clone()
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
                                return Err(error);
                            }
                        }
                    }
                    // Write-lock released here — flush can proceed under a read-lock.
                }

                trace!("Chunk data updated for {}", path.display());

                // We check watchers *after* releasing the write-lock to honour
                // lock ordering (serializer lock → watchers, never the reverse).
                let is_watched = {
                    let watchers = self.watchers.read().await;
                    watchers.get(&path).is_some_and(|&c| c > 0)
                };

                if !is_watched {
                    // A read-lock suffices for `write()` since we have already
                    // applied all mutations above.
                    {
                        let serializer = chunk_serializer.read().await;
                        debug!("Flushing {} to disk", path.display());
                        if let Err(error) = serializer.write(&path).await {
                            for chunk in &cleared_dirty {
                                chunk.mark_dirty(true);
                            }
                            return Err(ChunkWritingError::IoError(error));
                        }
                        // Read-lock released here.
                    };

                    // Drop our handle so `can_remove` may succeed.
                    drop(chunk_serializer);

                    // Evict the cache entry when no longer needed.
                    self.maybe_evict(&path).await;
                }

                Ok(())
            });

        // Collect all region results; surface the first error encountered.
        let results: Vec<Result<(), ChunkWritingError>> = join_all(tasks).await;
        results.into_iter().find(Result::is_err).unwrap_or(Ok(()))
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

#[cfg(all(test, windows))]
mod tests {
    use std::{fs, sync::Arc};

    use bytes::Bytes;
    use pumpkin_config::chunk::AnvilChunkConfig;
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
}
