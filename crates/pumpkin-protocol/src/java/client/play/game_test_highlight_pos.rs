use crate::{
    ClientPacket, ServerPacket, VarInt,
    ser::{NetworkReadExt, NetworkWriteExt, ReadingError, WritingError},
};
use pumpkin_data::packet::clientbound::play::GAME_TEST_HIGHLIGHT_POS;
use pumpkin_macros::java_packet;
use pumpkin_util::{math::position::BlockPos, version::JavaMinecraftVersion};

#[java_packet(GAME_TEST_HIGHLIGHT_POS)]
pub struct CGameTestHighlightPos {
    pub absolute_pos: BlockPos,
    pub relative_pos: BlockPos,
}

impl CGameTestHighlightPos {
    #[must_use]
    pub const fn new(absolute_pos: BlockPos, relative_pos: BlockPos) -> Self {
        Self {
            absolute_pos,
            relative_pos,
        }
    }
}

impl ClientPacket for CGameTestHighlightPos {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        for pos in [&self.absolute_pos, &self.relative_pos] {
            write.write_var_int(&VarInt(pos.0.x))?;
            write.write_var_int(&VarInt(pos.0.y))?;
            write.write_var_int(&VarInt(pos.0.z))?;
        }
        Ok(())
    }
}

impl<'a> ServerPacket<'a> for CGameTestHighlightPos {
    fn read(bytebuf: &mut &'a [u8], _version: &JavaMinecraftVersion) -> Result<Self, ReadingError> {
        let mut read_pos = || {
            Ok(BlockPos::new(
                bytebuf.get_var_int()?.0,
                bytebuf.get_var_int()?.0,
                bytebuf.get_var_int()?.0,
            ))
        };
        Ok(Self {
            absolute_pos: read_pos()?,
            relative_pos: read_pos()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_two_vec3i_positions_as_varints() {
        let packet = CGameTestHighlightPos::new(BlockPos::new(1, -2, 300), BlockPos::new(4, 5, 6));
        let mut bytes = Vec::new();
        packet
            .write_packet_data(&mut bytes, &JavaMinecraftVersion::V_26_2)
            .unwrap();
        assert_eq!(
            bytes,
            [1, 0xFE, 0xFF, 0xFF, 0xFF, 0x0F, 0xAC, 0x02, 4, 5, 6]
        );
        let mut input = bytes.as_slice();
        let decoded =
            CGameTestHighlightPos::read(&mut input, &JavaMinecraftVersion::V_26_2).unwrap();
        assert_eq!(decoded.absolute_pos, packet.absolute_pos);
        assert_eq!(decoded.relative_pos, packet.relative_pos);
        assert_eq!(input, &[]);
    }
}
