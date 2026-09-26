use crate::{
    ServerPacket,
    ser::{NetworkReadExt, ReadingError},
};
use pumpkin_data::packet::serverbound::play::MOVE_PLAYER_STATUS_ONLY;
use pumpkin_macros::java_packet;
use pumpkin_util::version::JavaMinecraftVersion;

#[java_packet(MOVE_PLAYER_STATUS_ONLY)]
pub struct SSetPlayerGround {
    pub on_ground: bool,
}

impl<'a> ServerPacket<'a> for SSetPlayerGround {
    fn read(bytebuf: &mut &'a [u8], _version: &JavaMinecraftVersion) -> Result<Self, ReadingError> {
        Ok(Self {
            on_ground: bytebuf.get_u8()? & 0x01 != 0,
        })
    }
}

impl crate::ClientPacket for SSetPlayerGround {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        _version: &JavaMinecraftVersion,
    ) -> Result<(), crate::ser::WritingError> {
        use crate::ser::NetworkWriteExt;
        write.write_bool(self.on_ground)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::java::server::play::SPlayerRotation;

    #[test]
    fn movement_on_ground_uses_only_flag_bit_zero() {
        let version = JavaMinecraftVersion::V_26_2;
        for flags in 0..=0x03 {
            let expected = flags & 0x01 != 0;

            let mut ground_bytes = [flags];
            assert_eq!(
                SSetPlayerGround::read(&mut ground_bytes.as_slice(), &version)
                    .unwrap()
                    .on_ground,
                expected,
                "status flags {flags:#04x}"
            );

            let mut rotation_bytes = [0.0_f32.to_be_bytes(), 0.0_f32.to_be_bytes()].concat();
            rotation_bytes.push(flags);
            assert_eq!(
                SPlayerRotation::read(&mut rotation_bytes.as_slice(), &version)
                    .unwrap()
                    .ground,
                expected,
                "rotation flags {flags:#04x}"
            );
        }
    }
}
