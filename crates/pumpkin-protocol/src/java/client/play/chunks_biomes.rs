use pumpkin_data::packet::clientbound::play::CHUNKS_BIOMES;
use pumpkin_macros::java_packet;

use crate::{ClientPacket, codec::var_int::VarInt, ser::NetworkWriteExt};
use pumpkin_util::version::JavaMinecraftVersion;

pub struct ChunkBiomeEntry<'a> {
    pub chunk_x: i32,
    pub chunk_z: i32,
    pub data: &'a [u8],
}

#[java_packet(CHUNKS_BIOMES)]
pub struct CChunksBiomes<'a> {
    pub chunks: &'a [ChunkBiomeEntry<'a>],
}

impl<'a> CChunksBiomes<'a> {
    #[must_use]
    pub const fn new(chunks: &'a [ChunkBiomeEntry<'a>]) -> Self {
        Self { chunks }
    }
}

impl ClientPacket for CChunksBiomes<'_> {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        _version: &JavaMinecraftVersion,
    ) -> Result<(), crate::ser::WritingError> {
        write.write_var_int(&VarInt(self.chunks.len() as i32))?;
        for chunk in self.chunks {
            write.write_i32_be(chunk.chunk_z)?;
            write.write_i32_be(chunk.chunk_x)?;
            write.write_var_int(&VarInt(chunk.data.len() as i32))?;
            write.write_slice(chunk.data)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_official_chunk_position_and_length_prefixed_biomes() {
        let data: Vec<_> = (0..128).collect();
        let chunks = [ChunkBiomeEntry {
            chunk_x: 0x0102_0304,
            chunk_z: 0x0506_0708,
            data: &data,
        }];
        let mut bytes = Vec::new();
        CChunksBiomes::new(&chunks)
            .write_packet_data(&mut bytes, &JavaMinecraftVersion::V_26_2)
            .unwrap();

        let mut expected: Vec<u8> = vec![
            0x01, // one chunk
            0x05, 0x06, 0x07, 0x08, // packed ChunkPos long, big-endian high (z) half
            0x01, 0x02, 0x03, 0x04, // low (x) half
            0x80, 0x01, // VarInt(128) byte-array length
        ];
        expected.extend(0..128);
        assert_eq!(bytes, expected);
    }
}
