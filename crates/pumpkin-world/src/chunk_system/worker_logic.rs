use super::chunk_state::{Chunk, StagedChunkEnum};
use super::generation_cache::Cache;
use super::{ChunkPos, IOLock};
use crate::ProtoChunk;
use crate::chunk::format::LightContainer;
use crate::chunk::io::{FileIO, LoadedData, run_blocking};
use crate::level::Level;
use pumpkin_config::lighting::LightingEngineConfig;
use pumpkin_data::chunk::ChunkStatus;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use tracing::{debug, error, warn};

pub(crate) enum IoWriteCommand {
    Chunks(Vec<(ChunkPos, Chunk)>),
    Barrier {
        completion: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
}

pub enum RecvChunk {
    IO(Chunk),
    /// A disk read/parse/decompression failure is terminal for this request.
    /// It must not be converted into a generated or dirty chunk.
    LoadFailure {
        pos: ChunkPos,
        error: String,
    },
    Generation(Cache),
    GenerationFailure {
        pos: ChunkPos,
        stage: StagedChunkEnum,
        error: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum LoadedChunkOutcome {
    Missing(ChunkPos),
    Error { pos: ChunkPos, error: String },
}

fn classify_loaded_data(
    data: LoadedData<Arc<crate::chunk::ChunkData>, crate::chunk::ChunkReadingError>,
) -> Result<Arc<crate::chunk::ChunkData>, LoadedChunkOutcome> {
    match data {
        LoadedData::Loaded(chunk) => Ok(chunk),
        LoadedData::Missing(pos) => Err(LoadedChunkOutcome::Missing(pos)),
        LoadedData::Error((pos, error)) => Err(LoadedChunkOutcome::Error {
            pos,
            error: error.to_string(),
        }),
    }
}

/// Checks if a chunk needs relighting based on the current lighting configuration
/// Returns true if the chunk has uniform lighting (from full/dark mode) but the server
/// is now running in default mode (which needs proper lighting calculation)
fn needs_relighting(chunk: &crate::chunk::ChunkData, config: LightingEngineConfig) -> bool {
    if config != LightingEngineConfig::Default {
        return false;
    }

    // If the chunk says it's already lit, believe it.
    if chunk.light_populated.load(Relaxed) {
        return false;
    }

    let engine = chunk
        .light_engine
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // Scan for any complex lighting data
    let has_complex_light = engine.sky_light.iter().any(|lc| match lc {
        LightContainer::Full(data) => data.iter().any(|&b| b != 0x00 && b != 0xFF),
        LightContainer::Empty(val) => *val != 0 && *val != 15,
    }) || engine.block_light.iter().any(|lc| match lc {
        LightContainer::Full(data) => data.iter().any(|&b| b != 0x00 && b != 0xFF),
        LightContainer::Empty(val) => *val != 0 && *val != 15,
    });

    // If it has complex light, we don't need to relight.
    !has_complex_light
}

fn load_proto_chunk(chunk: &crate::chunk::ChunkData, level: &Level) -> ProtoChunk {
    ProtoChunk::from_chunk_data(chunk, &level.world_gen.load())
}

fn process_loaded_chunk(chunk: Arc<crate::chunk::ChunkData>, level: &Level) -> Chunk {
    let pos = ChunkPos::new(chunk.x, chunk.z);
    if chunk.status == ChunkStatus::Full {
        let needs_relight = needs_relighting(&chunk, level.lighting_config);
        if needs_relight {
            debug!(
                "Chunk {pos:?} has uniform lighting, downgrading to Features stage for relighting"
            );

            let mut proto = load_proto_chunk(&chunk, level);

            // Clear all lighting data
            let section_count = proto.light.sky_light.len();
            proto.light.sky_light = (0..section_count)
                .map(|_| LightContainer::new_empty(15))
                .collect();
            proto.light.block_light = (0..section_count)
                .map(|_| LightContainer::new_empty(0))
                .collect();
            proto.stage = StagedChunkEnum::Features;
            Chunk::Proto(Box::new(proto))
        } else {
            Chunk::Level(chunk)
        }
    } else {
        let proto = load_proto_chunk(&chunk, level);
        Chunk::Proto(Box::new(proto))
    }
}

pub async fn io_read_work(
    recv: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Vec<ChunkPos>>>>,
    send: crossbeam::channel::Sender<(ChunkPos, RecvChunk)>,
    level: Arc<Level>,
    lock: IOLock,
) {
    debug!("io read thread start");

    // Cleaner loop and async recv
    loop {
        let batch = {
            let mut lock_rx = recv.lock().await;
            lock_rx.recv().await
        };
        let Some(batch) = batch else {
            break;
        };
        for pos in &batch {
            // Lock handling
            loop {
                let notified = lock.1.notified();
                if !lock
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(pos)
                {
                    break;
                }
                notified.await;
            }
        }

        // Keep the scheduler/worker contract one-result-per-coordinate even when
        // a FileIO implementation is batch-oriented. This also bounds the waiter
        // channel and prevents a missing terminal response from stranding the
        // scheduler indefinitely.
        for pos in batch {
            let requested = [pos];
            let (t_send, mut t_recv) = tokio::sync::mpsc::channel(1);
            level
                .chunk_saver
                .fetch_chunks(&level.level_folder, &requested, t_send)
                .await;

            let data = t_recv.recv().await.unwrap_or_else(|| {
                LoadedData::Error((
                    pos,
                    crate::chunk::ChunkReadingError::IoError(std::io::Error::other(
                        "chunk loader closed without a terminal outcome",
                    )),
                ))
            });

            let (result_pos, received) = match classify_loaded_data(data) {
                Ok(chunk) => {
                    let result_pos = ChunkPos::new(chunk.x, chunk.z);
                    if result_pos == pos {
                        let level = level.clone();
                        let result =
                            run_blocking(move || process_loaded_chunk(chunk, &level)).await;
                        let received = match result {
                            Ok(processed) => RecvChunk::IO(processed),
                            Err(err) => RecvChunk::GenerationFailure {
                                pos: result_pos,
                                stage: StagedChunkEnum::Empty,
                                error: err.to_string(),
                            },
                        };
                        (result_pos, received)
                    } else {
                        (
                            pos,
                            RecvChunk::LoadFailure {
                                pos,
                                error: format!(
                                    "Loaded chunk coordinates {result_pos:?} do not match requested {pos:?}"
                                ),
                            },
                        )
                    }
                }
                Err(LoadedChunkOutcome::Missing(result_pos)) => (
                    result_pos,
                    RecvChunk::IO(Chunk::Proto(Box::new(ProtoChunk::new(
                        result_pos.x,
                        result_pos.y,
                        &level.world_gen.load(),
                    )))),
                ),
                Err(LoadedChunkOutcome::Error {
                    pos: result_pos,
                    error,
                }) => (
                    result_pos,
                    RecvChunk::LoadFailure {
                        pos: result_pos,
                        error,
                    },
                ),
            };

            if send.send((result_pos, received)).is_err() {
                break;
            }
        }
    }
    debug!("io read thread stop");
}

pub(crate) async fn io_write_work(
    mut recv: tokio::sync::mpsc::Receiver<IoWriteCommand>,
    level: Arc<Level>,
    lock: IOLock,
) {
    let mut pending_error = None;

    while let Some(command) = recv.recv().await {
        match command {
            IoWriteCommand::Chunks(data) => {
                let positions = data.iter().map(|(pos, _)| *pos).collect::<Vec<_>>();
                let level_for_upgrade = level.clone();
                let result = match run_blocking(move || {
                    let mut vec = Vec::with_capacity(data.len());
                    for (pos, chunk) in data {
                        match chunk {
                            Chunk::Level(chunk) => vec.push((pos, chunk)),
                            Chunk::Proto(chunk) => {
                                let mut temp = Chunk::Proto(chunk);
                                temp.upgrade_to_level_chunk(
                                    level_for_upgrade.world_gen.load().dimension(),
                                    &level_for_upgrade.lighting_config,
                                );
                                let Chunk::Level(chunk) = temp else { panic!() };
                                vec.push((pos, chunk));
                            }
                        }
                    }
                    vec
                })
                .await
                {
                    Ok(chunks) => level
                        .chunk_saver
                        .save_chunks(&level.level_folder, chunks)
                        .await
                        .map_err(|error| format!("chunk write failed: {error}")),
                    Err(error) => Err(format!("chunk upgrade task failed: {error}")),
                };

                {
                    let mut data = lock
                        .0
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    for i in positions {
                        match data.entry(i) {
                            Entry::Occupied(mut entry) => {
                                let rc = entry.get_mut();
                                if *rc <= 1 {
                                    entry.remove();
                                } else {
                                    *rc -= 1;
                                }
                            }
                            Entry::Vacant(_) => {
                                warn!(
                                    "io_write: attempted to release missing lock entry for {:?}",
                                    i
                                );
                            }
                        }
                    }
                }
                lock.1.notify_waiters();

                if let Err(error) = result {
                    error!("Failed to save chunks: {error}");
                    pending_error.get_or_insert(error);
                }
            }
            IoWriteCommand::Barrier { completion } => {
                if level.shut_down_chunk_system.load(Relaxed) {
                    let _ = completion.send(Err("chunk save system is shutting down".to_string()));
                    continue;
                }
                level.chunk_saver.block_and_await_ongoing_tasks().await;
                let result = match pending_error.take() {
                    Some(error) => Err(error),
                    None => level
                        .chunk_saver
                        .sync_all(&level.level_folder)
                        .await
                        .map_err(|error| format!("chunk disk sync failed: {error}")),
                };
                let _ = completion.send(result);
            }
        }
    }
}

pub fn run_generation(
    pos: ChunkPos,
    mut cache: Cache,
    stage: StagedChunkEnum,
    level: &Level,
) -> RecvChunk {
    let portal = level.world_portal.load_full();
    let Some(portal_ref) = portal.as_deref() else {
        error!("Chunk generation FAILED at {pos:?} ({stage:?}): World portal is not initialized");
        return RecvChunk::GenerationFailure {
            pos,
            stage,
            error: "World portal is not initialized".to_string(),
        };
    };
    // Run generation with panic catching
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cache.advance(
            stage,
            &level.world_gen.load(),
            portal_ref,
            &level.lighting_config,
        );
        cache // Return cache on success
    }));

    match result {
        Ok(cache) => RecvChunk::Generation(cache),
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| {
                    payload
                        .downcast_ref::<String>()
                        .map(std::string::String::as_str)
                })
                .unwrap_or("Unknown panic payload");

            error!("Chunk generation FAILED at {pos:?} ({stage:?}): {msg}");

            RecvChunk::GenerationFailure {
                pos,
                stage,
                error: msg.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LoadedChunkOutcome, classify_loaded_data};
    use crate::chunk::{ChunkReadingError, io::LoadedData};
    use crate::chunk_system::ChunkPos;

    #[test]
    fn missing_load_is_classified_for_generation() {
        let pos = ChunkPos::new(-3, 7);
        let outcome = classify_loaded_data(LoadedData::Missing(pos));

        assert!(matches!(
            outcome,
            Err(LoadedChunkOutcome::Missing(found)) if found == pos
        ));
    }

    #[test]
    fn corrupt_load_is_classified_as_terminal_error() {
        let pos = ChunkPos::new(4, -9);
        let outcome =
            classify_loaded_data(LoadedData::Error((pos, ChunkReadingError::InvalidHeader)));

        assert!(matches!(
            outcome,
            Err(LoadedChunkOutcome::Error { pos: found, error })
                if found == pos && error == "Invalid header"
        ));
    }
}
