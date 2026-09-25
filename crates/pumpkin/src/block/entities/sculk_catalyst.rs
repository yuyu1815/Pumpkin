use super::BlockEntity;
use pumpkin_data::{
    BlockDirection, BlockState, BlockStateId, block_properties::is_air, fluid::Fluid,
};
use pumpkin_nbt::{NbtCompound, tag::NbtTag};
use pumpkin_util::{
    math::position::BlockPos,
    random::{RandomGenerator, xoroshiro128::Xoroshiro},
};
use pumpkin_world::{
    generation::feature::features::sculk::{
        SculkLevel,
        spreader::{ChargeCursor, SculkSpreader},
    },
    world::BlockFlags,
};
use std::sync::{Arc, Mutex};

use crate::world::World;

/// Runtime SculkLevel view that never loads chunks while cursors inspect them.
pub struct WorldSculkLevel<'a> {
    world: &'a Arc<World>,
}

impl<'a> WorldSculkLevel<'a> {
    #[must_use]
    pub const fn new(world: &'a Arc<World>) -> Self {
        Self { world }
    }
}

impl SculkLevel for WorldSculkLevel<'_> {
    fn sculk_get(&self, pos: BlockPos) -> Option<BlockStateId> {
        self.world.get_block_state_id_if_loaded(&pos)
    }

    fn sculk_set(&mut self, pos: BlockPos, state: &'static BlockState) {
        if self.world.is_loaded(&pos) {
            self.world
                .set_block_state(&pos, state.id, BlockFlags::NOTIFY_ALL);
        }
    }

    fn sculk_is_air(&self, pos: BlockPos) -> bool {
        self.sculk_get(pos).is_some_and(is_air)
    }

    fn sculk_is_water_source(&self, pos: BlockPos) -> bool {
        self.sculk_get(pos).is_some_and(|_| {
            let (fluid, state) = self.world.get_fluid_and_fluid_state(&pos);
            fluid == &Fluid::WATER && state.is_source
        })
    }

    fn sculk_is_water(&self, pos: BlockPos) -> bool {
        self.sculk_get(pos)
            .is_some_and(|_| self.world.get_fluid_and_fluid_state(&pos).0 == &Fluid::WATER)
    }

    fn sculk_is_face_sturdy(&self, pos: BlockPos, face: BlockDirection) -> bool {
        self.sculk_get(pos)
            .is_some_and(|id| id.to_state().is_side_solid(face))
    }

    fn sculk_is_full_cube(&self, pos: BlockPos) -> bool {
        self.sculk_get(pos)
            .is_some_and(|id| id.to_state().is_full_cube())
    }
}

pub struct SculkCatalystBlockEntity {
    pub position: BlockPos,
    pub spreader: Mutex<SculkSpreader>,
}

impl BlockEntity for SculkCatalystBlockEntity {
    fn tick(&self, world: &Arc<World>) {
        let Ok(mut spreader) = self.spreader.lock() else {
            return;
        };
        if spreader.cursors().is_empty() {
            return;
        }

        let mut level = WorldSculkLevel::new(world);
        let mut random = RandomGenerator::Xoroshiro(Xoroshiro::from_seed(rand::random()));
        spreader.update_cursors(&mut level, self.position, &mut random, true);
        drop(spreader);

        // Persist the updated block-entity NBT with the chunk, without broadcasting it.
        world
            .level
            .read_chunk_sync(&self.position.chunk_position(), |chunk| {
                chunk.mark_dirty(true)
            });
    }

    fn resource_location(&self) -> &'static str {
        Self::ID
    }

    fn get_position(&self) -> BlockPos {
        self.position
    }

    fn from_nbt(nbt: &NbtCompound, position: BlockPos) -> Self {
        let mut spreader = SculkSpreader::new_level_spreader();
        let cursors = nbt
            .get_list("cursors")
            .into_iter()
            .flatten()
            .filter_map(|tag| tag.extract_compound())
            .filter_map(read_cursor)
            .collect();
        spreader.set_cursors(cursors);
        Self {
            position,
            spreader: Mutex::new(spreader),
        }
    }

    fn write_nbt(&self, nbt: &mut NbtCompound) {
        if let Ok(spreader) = self.spreader.lock() {
            nbt.put_list("cursors", write_cursors(&spreader));
        }
    }

    fn chunk_data_nbt(&self) -> Option<NbtCompound> {
        let spreader = self.spreader.try_lock().ok()?;
        let mut nbt = NbtCompound::new();
        nbt.put_list("cursors", write_cursors(&spreader));
        Some(nbt)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn read_cursor(nbt: &NbtCompound) -> Option<ChargeCursor> {
    let [x, y, z] = nbt.get_int_array("pos")? else {
        return None;
    };
    let charge = u16::try_from(nbt.get_int("charge")?).ok()?.min(1000);
    let mut cursor = ChargeCursor::new(BlockPos::new(*x, *y, *z), charge);
    cursor.decay_delay = nbt.get_int("decay_delay").unwrap_or(1).clamp(0, 255) as u8;
    cursor.update_delay = nbt.get_int("update_delay").unwrap_or(0).clamp(0, 255) as u8;
    if let Some(facings) = nbt.get_list("facings") {
        let mut bits = 0;
        for tag in facings {
            if let Some(direction) = tag.extract_string().and_then(parse_direction) {
                bits |= 1 << direction.to_index();
            }
        }
        cursor.faces = Some(bits);
    }
    Some(cursor)
}

fn parse_direction(value: &str) -> Option<pumpkin_data::BlockDirection> {
    use pumpkin_data::BlockDirection as D;
    Some(match value {
        "down" => D::Down,
        "up" => D::Up,
        "north" => D::North,
        "south" => D::South,
        "west" => D::West,
        "east" => D::East,
        _ => return None,
    })
}

fn direction_name(direction: pumpkin_data::BlockDirection) -> &'static str {
    use pumpkin_data::BlockDirection as D;
    match direction {
        D::Down => "down",
        D::Up => "up",
        D::North => "north",
        D::South => "south",
        D::West => "west",
        D::East => "east",
    }
}

fn write_cursors(spreader: &SculkSpreader) -> Vec<NbtTag> {
    spreader
        .cursors()
        .iter()
        .map(|cursor| {
            let mut nbt = NbtCompound::new();
            nbt.put(
                "pos",
                NbtTag::IntArray(vec![cursor.pos.0.x, cursor.pos.0.y, cursor.pos.0.z]),
            );
            nbt.put_int("charge", i32::from(cursor.charge));
            nbt.put_int("decay_delay", i32::from(cursor.decay_delay));
            nbt.put_int("update_delay", i32::from(cursor.update_delay));
            if let Some(faces) = cursor.faces {
                nbt.put_list(
                    "facings",
                    pumpkin_data::BlockDirection::all()
                        .into_iter()
                        .filter(|direction| faces & (1 << direction.to_index()) != 0)
                        .map(|direction| NbtTag::String(direction_name(direction).to_owned()))
                        .collect(),
                );
            }
            NbtTag::Compound(nbt)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_nbt_preserves_optional_faces_and_clamps_limits() {
        let mut spreader = SculkSpreader::new_level_spreader();
        let mut absent = ChargeCursor::new(BlockPos::new(1, 2, 3), 12);
        absent.decay_delay = 4;
        absent.update_delay = 5;
        let mut empty = ChargeCursor::new(BlockPos::new(4, 5, 6), 20);
        empty.faces = Some(0);
        let mut directions = ChargeCursor::new(BlockPos::new(7, 8, 9), 1000);
        directions.faces = Some(1 | (1 << pumpkin_data::BlockDirection::East.to_index()));
        spreader.set_cursors(vec![absent, empty, directions]);

        let decoded: Vec<_> = write_cursors(&spreader)
            .iter()
            .filter_map(NbtTag::extract_compound)
            .filter_map(read_cursor)
            .collect();
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0].faces, None);
        assert_eq!(decoded[0].decay_delay, 4);
        assert_eq!(decoded[0].update_delay, 5);
        assert_eq!(decoded[1].faces, Some(0));
        assert_eq!(decoded[2].faces, Some(1 | (1 << 5)));
        assert_eq!(decoded[2].charge, 1000);
    }
}

impl SculkCatalystBlockEntity {
    pub const ID: &'static str = "minecraft:sculk_catalyst";

    #[must_use]
    pub fn new(position: BlockPos) -> Self {
        Self {
            position,
            spreader: Mutex::new(SculkSpreader::new_level_spreader()),
        }
    }
}
