use pumpkin_protocol::codec::bit_set::BitSet;
use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_protocol::java::client::play::{CLightUpdate, LightData};
use pumpkin_protocol::ser::WritingError;
use pumpkin_util::version::JavaMinecraftVersion;
use pumpkin_world::chunk::ChunkData;
use pumpkin_world::chunk::format::LightContainer;

pub trait ChunkLightExt {
    fn from_chunk(chunk: &ChunkData, version: JavaMinecraftVersion) -> Result<Self, WritingError>
    where
        Self: Sized;
}

impl ChunkLightExt for CLightUpdate {
    fn from_chunk(chunk: &ChunkData, version: JavaMinecraftVersion) -> Result<Self, WritingError> {
        let light_data = light_data_from_chunk(chunk, version)?;
        Ok(Self {
            chunk_x: VarInt(chunk.x),
            chunk_z: VarInt(chunk.z),
            light_data,
        })
    }
}

fn section_payload(container: &LightContainer) -> (bool, Option<Vec<u8>>) {
    match container {
        LightContainer::Full(data) => (false, Some(data.to_vec())),
        LightContainer::Empty(value) if *value > 0 => (false, Some(vec![*value << 4 | *value; 2048])),
        LightContainer::Empty(_) => (true, None),
    }
}

fn protocol_bit_index(
    version: JavaMinecraftVersion,
    base_section: usize,
    index: usize,
) -> Option<usize> {
    if version >= JavaMinecraftVersion::V_1_18 {
        Some(index + 1)
    } else if index + 1 == base_section {
        Some(0)
    } else if index >= base_section && index < base_section + 16 {
        Some(index - base_section + 1)
    } else if index == base_section + 16 {
        Some(17)
    } else {
        None
    }
}

/// Builds a light update containing only the changed light channel/sections.
/// `section_index` is the chunk's zero-based section index; protocol bit zero is below the world.
pub fn light_data_for_sections(
    chunk: &ChunkData,
    version: JavaMinecraftVersion,
    sections: &[(usize, bool)],
) -> Result<LightData, WritingError> {
    let light_engine = chunk
        .light_engine
        .lock()
        .map_err(|_| WritingError::Message("light_engine lock poisoned".into()))?;
    let mut sky_light_mask = 0u64;
    let mut block_light_mask = 0u64;
    let mut empty_sky_light_mask = 0u64;
    let mut empty_block_light_mask = 0u64;
    let mut sky_light_arrays = Vec::new();
    let mut block_light_arrays = Vec::new();

    let base_section = (0 - chunk.section.min_y).max(0) as usize / 16;
    let mut sections = sections.to_vec();
    sections.sort_by_key(|(index, _)| {
        protocol_bit_index(version, base_section, *index).unwrap_or(usize::MAX)
    });
    for &(index, is_sky) in &sections {
        let Some(container) = (if is_sky {
            light_engine.sky_light.get(index)
        } else {
            light_engine.block_light.get(index)
        }) else {
            continue;
        };
        let Some(bit_index) = protocol_bit_index(version, base_section, index) else {
            continue;
        };
        let bit = 1u64 << bit_index;
        let (mask, empty_mask, arrays) = if is_sky {
            (&mut sky_light_mask, &mut empty_sky_light_mask, &mut sky_light_arrays)
        } else {
            (&mut block_light_mask, &mut empty_block_light_mask, &mut block_light_arrays)
        };
        let (empty, data) = section_payload(container);
        if empty {
            *empty_mask |= bit;
        }
        if let Some(data) = data {
            *mask |= bit;
            arrays.push(data);
        }
    }
    Ok(LightData {
        trust_edges: true,
        sky_light_mask: BitSet::from_u64(sky_light_mask),
        block_light_mask: BitSet::from_u64(block_light_mask),
        empty_sky_light_mask: BitSet::from_u64(empty_sky_light_mask),
        empty_block_light_mask: BitSet::from_u64(empty_block_light_mask),
        sky_light_arrays,
        block_light_arrays,
    })
}

#[cfg(test)]
mod incremental_tests {
    use super::{light_data_for_sections, section_payload};
    use pumpkin_util::version::JavaMinecraftVersion;
    use pumpkin_world::chunk::format::LightContainer;
    use pumpkin_world::chunk::{ChunkData, ChunkLight};

    #[test]
    fn incremental_section_masks_distinguish_empty_and_filled_empty_containers() {
        let (empty, data) = section_payload(&LightContainer::Empty(0));
        assert!(empty);
        assert!(data.is_none());

        let (empty, data) = section_payload(&LightContainer::Empty(7));
        assert!(!empty);
        let data = data.unwrap();
        assert_eq!(data.len(), 2048);
        assert!(data.iter().all(|byte| *byte == 0x77));
    }

    #[test]
    fn incremental_light_arrays_follow_ascending_protocol_mask_bits() {
        let chunk = ChunkData::empty(0, 0);
        *chunk.light_engine.lock().unwrap() = ChunkLight {
            sky_light: vec![
                LightContainer::Full(vec![0x11; 2048].into_boxed_slice()),
                LightContainer::Full(vec![0x22; 2048].into_boxed_slice()),
            ]
            .into_boxed_slice(),
            block_light: vec![
                LightContainer::Full(vec![0x33; 2048].into_boxed_slice()),
                LightContainer::Full(vec![0x44; 2048].into_boxed_slice()),
            ]
            .into_boxed_slice(),
        };
        let version = JavaMinecraftVersion::V_1_18;
        let light = light_data_for_sections(
            &chunk,
            version,
            &[(1, true), (0, false), (0, true), (1, false)],
        )
        .unwrap();

        let mut bytes = Vec::new();
        light.write(&mut bytes, &version).unwrap();
        let decoded = pumpkin_protocol::java::client::play::LightData::read(
            &mut bytes.as_slice(),
            &version,
        )
        .unwrap();

        assert_eq!(decoded.sky_light_mask.as_u64(), 0b110);
        assert_eq!(decoded.block_light_mask.as_u64(), 0b110);
        assert_eq!(decoded.sky_light_arrays, vec![vec![0x11; 2048], vec![0x22; 2048]]);
        assert_eq!(decoded.block_light_arrays, vec![vec![0x33; 2048], vec![0x44; 2048]]);
    }
}

pub fn light_data_from_chunk(
    chunk: &ChunkData,
    version: JavaMinecraftVersion,
) -> Result<LightData, WritingError> {
    let light_engine = chunk
        .light_engine
        .lock()
        .map_err(|_| WritingError::Message("light_engine lock poisoned".into()))?;

    if version < JavaMinecraftVersion::V_1_18 {
        let base_section = (0 - chunk.section.min_y).max(0) as usize / 16;
        let mut sky_light_mask = 0u64;
        let mut block_light_mask = 0u64;
        let mut sky_light_empty_mask = 0u64;
        let mut block_light_empty_mask = 0u64;
        let mut sky_light_arrays = Vec::new();
        let mut block_light_arrays = Vec::new();

        // Bit 0: Y = -1 (below world section 0)
        if base_section > 0 && base_section - 1 < light_engine.sky_light.len() {
            match &light_engine.sky_light[base_section - 1] {
                LightContainer::Full(data) => {
                    sky_light_mask |= 1 << 0;
                    sky_light_arrays.push(data.to_vec());
                }
                LightContainer::Empty(val) if *val > 0 => {
                    sky_light_mask |= 1 << 0;
                    sky_light_arrays.push(vec![*val << 4 | *val; 2048]);
                }
                LightContainer::Empty(_) => {
                    sky_light_empty_mask |= 1 << 0;
                }
            }
        } else {
            sky_light_empty_mask |= 1 << 0;
        }

        if base_section > 0 && base_section - 1 < light_engine.block_light.len() {
            match &light_engine.block_light[base_section - 1] {
                LightContainer::Full(data) => {
                    block_light_mask |= 1 << 0;
                    block_light_arrays.push(data.to_vec());
                }
                LightContainer::Empty(val) if *val > 0 => {
                    block_light_mask |= 1 << 0;
                    block_light_arrays.push(vec![*val << 4 | *val; 2048]);
                }
                LightContainer::Empty(_) => {
                    block_light_empty_mask |= 1 << 0;
                }
            }
        } else {
            block_light_empty_mask |= 1 << 0;
        }

        // Bits 1..=16: world sections (Y = 0..15)
        for i in 0..16 {
            let bit_index = i + 1;
            let sec_idx = base_section + i;

            if sec_idx < light_engine.sky_light.len() {
                match &light_engine.sky_light[sec_idx] {
                    LightContainer::Full(data) => {
                        sky_light_mask |= 1 << bit_index;
                        sky_light_arrays.push(data.to_vec());
                    }
                    LightContainer::Empty(val) if *val > 0 => {
                        sky_light_mask |= 1 << bit_index;
                        sky_light_arrays.push(vec![*val << 4 | *val; 2048]);
                    }
                    LightContainer::Empty(_) => {
                        sky_light_empty_mask |= 1 << bit_index;
                    }
                }
            } else {
                sky_light_empty_mask |= 1 << bit_index;
            }

            if sec_idx < light_engine.block_light.len() {
                match &light_engine.block_light[sec_idx] {
                    LightContainer::Full(data) => {
                        block_light_mask |= 1 << bit_index;
                        block_light_arrays.push(data.to_vec());
                    }
                    LightContainer::Empty(val) if *val > 0 => {
                        block_light_mask |= 1 << bit_index;
                        block_light_arrays.push(vec![*val << 4 | *val; 2048]);
                    }
                    LightContainer::Empty(_) => {
                        block_light_empty_mask |= 1 << bit_index;
                    }
                }
            } else {
                block_light_empty_mask |= 1 << bit_index;
            }
        }

        // Bit 17: Y = 16 (above world section 15)
        let top_sec = base_section + 16;
        if top_sec < light_engine.sky_light.len() {
            match &light_engine.sky_light[top_sec] {
                LightContainer::Full(data) => {
                    sky_light_mask |= 1 << 17;
                    sky_light_arrays.push(data.to_vec());
                }
                LightContainer::Empty(val) if *val > 0 => {
                    sky_light_mask |= 1 << 17;
                    sky_light_arrays.push(vec![*val << 4 | *val; 2048]);
                }
                LightContainer::Empty(_) => {
                    sky_light_empty_mask |= 1 << 17;
                }
            }
        } else {
            sky_light_empty_mask |= 1 << 17;
        }

        if top_sec < light_engine.block_light.len() {
            match &light_engine.block_light[top_sec] {
                LightContainer::Full(data) => {
                    block_light_mask |= 1 << 17;
                    block_light_arrays.push(data.to_vec());
                }
                LightContainer::Empty(val) if *val > 0 => {
                    block_light_mask |= 1 << 17;
                    block_light_arrays.push(vec![*val << 4 | *val; 2048]);
                }
                LightContainer::Empty(_) => {
                    block_light_empty_mask |= 1 << 17;
                }
            }
        } else {
            block_light_empty_mask |= 1 << 17;
        }

        Ok(LightData {
            trust_edges: true,
            sky_light_mask: BitSet::from_u64(sky_light_mask),
            block_light_mask: BitSet::from_u64(block_light_mask),
            empty_sky_light_mask: BitSet::from_u64(sky_light_empty_mask),
            empty_block_light_mask: BitSet::from_u64(block_light_empty_mask),
            sky_light_arrays,
            block_light_arrays,
        })
    } else {
        let num_sections = light_engine.sky_light.len();
        let mut sky_light_empty_mask = 0u64;
        let mut block_light_empty_mask = 0u64;
        let mut sky_light_mask = 0u64;
        let mut block_light_mask = 0u64;

        let mut sky_light_arrays = Vec::new();
        let mut block_light_arrays = Vec::new();

        sky_light_empty_mask |= 1 << 0;
        block_light_empty_mask |= 1 << 0;

        for section_index in 0..num_sections {
            let bit_index = section_index + 1;

            if let LightContainer::Full(data) = &light_engine.sky_light[section_index] {
                sky_light_mask |= 1 << bit_index;
                sky_light_arrays.push(data.to_vec());
            } else {
                sky_light_empty_mask |= 1 << bit_index;
            }

            if let LightContainer::Full(data) = &light_engine.block_light[section_index] {
                block_light_mask |= 1 << bit_index;
                block_light_arrays.push(data.to_vec());
            } else {
                block_light_empty_mask |= 1 << bit_index;
            }
        }

        sky_light_empty_mask |= 1 << (num_sections + 1);
        block_light_empty_mask |= 1 << (num_sections + 1);

        Ok(LightData {
            trust_edges: true,
            sky_light_mask: BitSet::from_u64(sky_light_mask),
            block_light_mask: BitSet::from_u64(block_light_mask),
            empty_sky_light_mask: BitSet::from_u64(sky_light_empty_mask),
            empty_block_light_mask: BitSet::from_u64(block_light_empty_mask),
            sky_light_arrays,
            block_light_arrays,
        })
    }
}
