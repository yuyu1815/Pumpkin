use pumpkin_data::packet::clientbound::config::DISCONNECT;
use pumpkin_macros::java_packet;
use pumpkin_util::text::TextComponent;
use pumpkin_util::version::JavaMinecraftVersion;

use crate::ClientPacket;
use crate::ser::NetworkWriteExt;

#[java_packet(DISCONNECT)]
pub struct CConfigDisconnect<'a> {
    pub reason: &'a TextComponent,
}

impl<'a> CConfigDisconnect<'a> {
    #[must_use]
    pub const fn new(reason: &'a TextComponent) -> Self {
        Self { reason }
    }
}

impl ClientPacket for CConfigDisconnect<'_> {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), crate::ser::WritingError> {
        write.write_component(self.reason, version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_disconnect_uses_component_wire_codec_for_version() {
        let reason = TextComponent::text("kicked");
        let packet = CConfigDisconnect::new(&reason);

        let mut legacy = Vec::new();
        packet
            .write_packet_data(&mut legacy, &JavaMinecraftVersion::V_1_20_2)
            .unwrap();
        assert_eq!(legacy, b"\x11{\"text\":\"kicked\"}");

        let mut current = Vec::new();
        packet
            .write_packet_data(&mut current, &JavaMinecraftVersion::V_26_2)
            .unwrap();
        // The trusted context-free codec uses unnamed NBT (`writeAnyTag`), not a root-name field.
        assert_eq!(current, &[0x08, 0, 6, b'k', b'i', b'c', b'k', b'e', b'd']);
    }
}
