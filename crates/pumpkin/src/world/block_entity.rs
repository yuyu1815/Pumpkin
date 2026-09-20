use super::{World, bedrock_chest_block_actor};
use crate::block::entities::{BlockEntity, block_entity_from_nbt, block_entity_matches_state};
use pumpkin_data::BlockStateId;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_protocol::java::client::play::CBlockEntityData;
use pumpkin_util::math::{position::BlockPos, vector2::Vector2};
use pumpkin_world::{chunk::ChunkData, chunk::io::Dirtiable};
use rustc_hash::FxHashMap;
use std::sync::Arc;

impl World {
    /// Sends output-signal updates for block entities changed this tick, in list order.
    pub(super) fn flush_comparator_updates(
        self: &Arc<Self>,
        block_entities: &[Arc<dyn BlockEntity>],
    ) {
        for be in block_entities {
            // Vanilla `BlockEntity.setChanged` -> `Level.updateNeighbourForOutputSignal`.
            if !be.is_comparator_dirty() {
                continue;
            }
            be.clear_comparator_dirty();
            let pos = be.get_position();
            // The list is a snapshot, so the chunk may be gone by now.
            if let Some(state_id) = self.get_block_state_id_if_loaded(&pos) {
                self.update_neighbour_for_output_signal(&pos, state_id.to_block());
            }
        }
    }

    pub fn get_block_entity(&self, block_pos: &BlockPos) -> Option<Arc<dyn BlockEntity>> {
        let chunk_pos = block_pos.chunk_position();
        if let Some(entity) = self
            .block_entities
            .get(&chunk_pos)
            .and_then(|m| m.get(block_pos).cloned())
        {
            let state_id = self.get_block_state_id_if_loaded(block_pos)?;
            return block_entity_matches_state(entity.as_ref(), state_id).then_some(entity);
        }

        let nbt = self
            .level
            .read_chunk_sync(&chunk_pos, |chunk| {
                chunk
                    .pending_block_entities
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(block_pos)
                    .cloned()
            })
            .flatten()?;
        let state_id = self.get_block_state_id_if_loaded(block_pos)?;
        let entity = block_entity_from_nbt(&nbt)?;
        if !block_entity_matches_state(entity.as_ref(), state_id) {
            return None;
        }
        if let Some(custom_data) = nbt
            .get_compound("PumpkinCustomData")
            .or_else(|| nbt.get_compound("BukkitValues"))
        {
            self.custom_block_entity_data
                .insert(*block_pos, custom_data.clone());
        }
        self.block_entities
            .entry(chunk_pos)
            .or_default()
            .insert(*block_pos, entity.clone());
        Some(entity)
    }

    pub(super) fn bedrock_block_entity_data(
        &self,
        state_id: BlockStateId,
        position: BlockPos,
    ) -> Option<NbtCompound> {
        self.get_block_entity(&position)?
            .bedrock_block_actor_data(state_id)
    }

    /// Builds Bedrock block actor tags that are not represented by Java block states alone.
    pub fn bedrock_chunk_block_actors(&self, chunk: &ChunkData) -> Vec<NbtCompound> {
        let chunk_pos = Vector2::new(chunk.x, chunk.z);
        let live_entities: FxHashMap<_, _> = self
            .block_entities
            .get(&chunk_pos)
            .map(|entities| {
                entities
                    .iter()
                    .map(|(position, entity)| (*position, entity.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let pending = chunk
            .pending_block_entities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        live_entities
            .iter()
            .filter_map(|(position, entity)| {
                let relative = position.chunk_relative_position();
                chunk
                    .section
                    .get_block_absolute_y(relative.x as usize, relative.y, relative.z as usize)
                    .and_then(|state_id| {
                        block_entity_matches_state(entity.as_ref(), state_id)
                            .then(|| {
                                bedrock_chest_block_actor(state_id, *position)
                                    .or_else(|| entity.bedrock_block_actor_data(state_id))
                            })
                            .flatten()
                    })
            })
            .chain(
                pending
                    .iter()
                    .filter(|(position, _)| !live_entities.contains_key(position))
                    .filter_map(|(position, nbt)| {
                        let relative = position.chunk_relative_position();
                        let state_id = chunk.section.get_block_absolute_y(
                            relative.x as usize,
                            relative.y,
                            relative.z as usize,
                        )?;
                        bedrock_chest_block_actor(state_id, *position).or_else(|| {
                            let entity = block_entity_from_nbt(nbt)?;
                            block_entity_matches_state(entity.as_ref(), state_id)
                                .then(|| entity.bedrock_block_actor_data(state_id))
                                .flatten()
                        })
                    }),
            )
            .collect()
    }

    pub fn add_block_entity(&self, block_entity: Arc<dyn BlockEntity>) {
        let block_pos = block_entity.get_position();
        let chunk_pos = block_pos.chunk_position();
        let block_entity_nbt = block_entity.chunk_data_nbt();
        let entity_id = block_entity.resource_location().to_string();

        if let Some(nbt) = &block_entity_nbt {
            let bytes = pumpkin_nbt::Nbt::from(nbt.clone()).write_unnamed();
            self.broadcast_to_chunk(
                chunk_pos,
                &CBlockEntityData::new(
                    block_entity.get_position(),
                    VarInt(block_entity.get_id() as i32),
                    bytes.as_ref().into(),
                ),
            );
        }

        self.block_entities
            .entry(chunk_pos)
            .or_default()
            .insert(block_pos, block_entity);

        if let Some(nbt) = block_entity_nbt {
            let mut full_nbt = nbt;
            full_nbt.put_string("id", entity_id);
            full_nbt.put_int("x", block_pos.0.x);
            full_nbt.put_int("y", block_pos.0.y);
            full_nbt.put_int("z", block_pos.0.z);
            self.add_block_entity_nbt(block_pos, &full_nbt);
        }

        self.level.read_chunk_sync(&chunk_pos, |chunk| {
            chunk.mark_dirty(true);
        });
    }

    pub(crate) fn add_block_entity_nbt(&self, block_pos: BlockPos, nbt: &NbtCompound) {
        if self
            .level
            .read_chunk_sync(&block_pos.chunk_position(), |chunk| {
                chunk
                    .pending_block_entities
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(block_pos, nbt.clone());
                chunk.mark_dirty(true);
            })
            .is_some()
        {
            self.pending_block_entity_migrations
                .push(block_pos.chunk_position());
        }
    }

    pub fn remove_block_entity(&self, block_pos: &BlockPos) {
        let chunk_pos = block_pos.chunk_position();
        let removed_live =
            self.block_entities
                .get_mut(&chunk_pos)
                .is_some_and(|mut chunk_block_entities| {
                    chunk_block_entities.remove(block_pos).is_some()
                });
        let removed_pending = self
            .level
            .read_chunk_sync(&chunk_pos, |chunk| {
                chunk
                    .pending_block_entities
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(block_pos)
                    .is_some()
            })
            .unwrap_or(false);

        if removed_live || removed_pending {
            self.custom_block_entity_data.remove(block_pos);
            // Drop the chunk's map once its last block entity is gone.
            self.block_entities
                .remove_if(&chunk_pos, |_, entities| entities.is_empty());
            self.level.read_chunk_sync(&chunk_pos, |chunk| {
                chunk.mark_dirty(true);
            });
        }
    }

    pub(super) fn migrate_pending_block_entities(&self, chunk_pos: Vector2<i32>) {
        let positions: Vec<BlockPos> = self
            .level
            .read_chunk_sync(&chunk_pos, |chunk| {
                chunk
                    .pending_block_entities
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .keys()
                    .copied()
                    .collect()
            })
            .unwrap_or_default();
        for pos in positions {
            let already_loaded = self
                .block_entities
                .get(&chunk_pos)
                .is_some_and(|m| m.contains_key(&pos));
            if !already_loaded && let Some(entity) = self.get_block_entity(&pos) {
                self.update_block_entity(&entity);
            }
        }
    }

    pub fn update_block_entity(&self, block_entity: &Arc<dyn BlockEntity>) {
        let block_pos = block_entity.get_position();
        let chunk_pos = block_pos.chunk_position();
        let block_entity_nbt = block_entity.chunk_data_nbt();

        if let Some(nbt) = &block_entity_nbt {
            let bytes = pumpkin_nbt::Nbt::from(nbt.clone()).write_unnamed();
            self.broadcast_to_chunk(
                chunk_pos,
                &CBlockEntityData::new(
                    block_entity.get_position(),
                    VarInt(block_entity.get_id() as i32),
                    bytes.as_ref().into(),
                ),
            );
            let mut full_nbt = nbt.clone();
            full_nbt.put_string("id", block_entity.resource_location().to_string());
            let pos = block_entity.get_position();
            full_nbt.put_int("x", pos.0.x);
            full_nbt.put_int("y", pos.0.y);
            full_nbt.put_int("z", pos.0.z);
            self.add_block_entity_nbt(block_pos, &full_nbt);
        }
        self.level.read_chunk_sync(&chunk_pos, |chunk| {
            chunk.mark_dirty(true);
        });
    }

    /// Serializes the live block entities of a chunk back into that chunk's block
    /// entity data. The live map is the source of truth while a chunk is loaded -
    /// `get_block_entity` reads the saved NBT from the chunk when it wakes an
    /// entity up - so this has to run before the chunk is dropped, or everything
    /// the entity did since it was loaded is lost.
    pub(super) fn save_block_entities(&self, chunk_pos: Vector2<i32>) {
        let Some(block_entities) = self
            .block_entities
            .get(&chunk_pos)
            .map(|chunk_block_entities| chunk_block_entities.values().cloned().collect::<Vec<_>>())
        else {
            return;
        };

        for block_entity in block_entities {
            let Some(state_id) = self.get_block_state_id_if_loaded(&block_entity.get_position())
            else {
                continue;
            };
            if !block_entity_matches_state(block_entity.as_ref(), state_id) {
                continue;
            }
            let mut nbt = NbtCompound::new();
            block_entity.write_internal(&mut nbt);
            if let Some(custom_data) = self
                .custom_block_entity_data
                .get(&block_entity.get_position())
                && !custom_data.is_empty()
            {
                nbt.put_compound("PumpkinCustomData", custom_data.clone());
            }
            self.add_block_entity_nbt(block_entity.get_position(), &nbt);
        }
    }

    pub fn set_block_entity_custom_data(
        &self,
        pos: &BlockPos,
        namespace: &str,
        key: &str,
        value: pumpkin_nbt::tag::NbtTag,
    ) {
        let mut entry = self.custom_block_entity_data.entry(*pos).or_default();
        let mut namespace_data = entry
            .child_tags
            .remove(namespace)
            .and_then(|tag| match tag {
                pumpkin_nbt::tag::NbtTag::Compound(compound) => Some(compound),
                _ => None,
            })
            .unwrap_or_default();

        namespace_data.child_tags.insert(key.into(), value);
        entry.child_tags.insert(
            namespace.into(),
            pumpkin_nbt::tag::NbtTag::Compound(namespace_data),
        );
    }

    pub fn get_block_entity_custom_data(
        &self,
        pos: &BlockPos,
        namespace: &str,
        key: &str,
    ) -> Option<pumpkin_nbt::tag::NbtTag> {
        self.custom_block_entity_data
            .get(pos)?
            .get(namespace)?
            .extract_compound()?
            .get(key)
            .cloned()
    }

    pub fn remove_block_entity_custom_data(&self, pos: &BlockPos, namespace: &str, key: &str) {
        if let Some(mut entry) = self.custom_block_entity_data.get_mut(pos) {
            let Some(pumpkin_nbt::tag::NbtTag::Compound(mut namespace_data)) =
                entry.child_tags.remove(namespace)
            else {
                return;
            };

            namespace_data.child_tags.remove(key);
            if !namespace_data.is_empty() {
                entry.child_tags.insert(
                    namespace.into(),
                    pumpkin_nbt::tag::NbtTag::Compound(namespace_data),
                );
            }
        }
    }

    pub fn has_block_entity_custom_data(&self, pos: &BlockPos, namespace: &str, key: &str) -> bool {
        self.get_block_entity_custom_data(pos, namespace, key)
            .is_some()
    }
}
