use pumpkin_data::packet::clientbound::config::KEEP_ALIVE;
use pumpkin_macros::java_packet;

use crate::ClientPacket;
use crate::ser::NetworkWriteExt;
use pumpkin_util::version::JavaMinecraftVersion;

/// Maintains the connection while the client is in the Configuration state.
#[java_packet(KEEP_ALIVE)]
pub struct CKeepAlive {
    pub keep_alive_id: i64,
}

impl CKeepAlive {
    #[must_use]
    pub const fn new(keep_alive_id: i64) -> Self {
        Self { keep_alive_id }
    }
}

impl ClientPacket for CKeepAlive {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        _version: &JavaMinecraftVersion,
    ) -> Result<(), crate::ser::WritingError> {
        write.write_i64_be(self.keep_alive_id)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ServerPacket, java::server::config::SKeepAlive};

    #[test]
    fn configuration_keep_alive_uses_a_big_endian_long() {
        let version = JavaMinecraftVersion::V_26_2;
        let packet = CKeepAlive::new(0x0102_0304_0506_0708);
        let mut payload = Vec::new();
        packet.write_packet_data(&mut payload, &version).unwrap();
        assert_eq!(payload, [1, 2, 3, 4, 5, 6, 7, 8]);

        let mut read = payload.as_slice();
        assert_eq!(
            SKeepAlive::read(&mut read, &version).unwrap().keep_alive_id,
            packet.keep_alive_id
        );
        assert!(read.is_empty());
    }
}
