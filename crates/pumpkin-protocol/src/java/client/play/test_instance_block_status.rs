use crate::{
    ClientPacket, ServerPacket, VarInt,
    ser::{NetworkReadSliceExt, NetworkWriteExt, ReadingError, WritingError},
};
use pumpkin_data::packet::clientbound::play::TEST_INSTANCE_BLOCK_STATUS;
use pumpkin_macros::java_packet;
use pumpkin_util::{math::position::BlockPos, text::TextComponent, version::JavaMinecraftVersion};

#[java_packet(TEST_INSTANCE_BLOCK_STATUS)]
pub struct CTestInstanceBlockStatus {
    pub status: TextComponent,
    pub size: Option<BlockPos>,
}

impl CTestInstanceBlockStatus {
    #[must_use]
    pub const fn new(status: TextComponent, size: Option<BlockPos>) -> Self {
        Self { status, size }
    }
}

impl ClientPacket for CTestInstanceBlockStatus {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        write.write_component(&self.status, version)?;
        write.write_option(&self.size, |write, pos| {
            write.write_var_int(&VarInt(pos.0.x))?;
            write.write_var_int(&VarInt(pos.0.y))?;
            write.write_var_int(&VarInt(pos.0.z))
        })?;
        Ok(())
    }
}

impl<'a> ServerPacket<'a> for CTestInstanceBlockStatus {
    fn read(bytebuf: &mut &'a [u8], version: &JavaMinecraftVersion) -> Result<Self, ReadingError> {
        let status = bytebuf.get_component(version)?;
        let size = bytebuf.get_option(|buf| {
            Ok(BlockPos::new(
                buf.get_var_int()?.0,
                buf.get_var_int()?.0,
                buf.get_var_int()?.0,
            ))
        })?;
        Ok(Self { status, size })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_component_and_optional_vec3i_size() {
        let component = TextComponent::text("ready");
        let packet =
            CTestInstanceBlockStatus::new(component.clone(), Some(BlockPos::new(2, -1, 3)));
        let version = JavaMinecraftVersion::V_26_2;
        let mut bytes = Vec::new();
        packet.write_packet_data(&mut bytes, &version).unwrap();

        let mut expected = component.encode_for_version(&version).to_vec();
        expected.extend_from_slice(&[1, 2, 0xFF, 0xFF, 0xFF, 0x0F, 3]);
        assert_eq!(bytes, expected);

        let mut input = bytes.as_slice();
        let decoded = CTestInstanceBlockStatus::read(&mut input, &version).unwrap();
        assert_eq!(decoded.size, packet.size);
        assert_eq!(input, &[]);
    }
}
