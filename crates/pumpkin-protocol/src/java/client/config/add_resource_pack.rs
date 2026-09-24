use pumpkin_util::text::TextComponent;

use pumpkin_macros::java_packet;

use pumpkin_data::packet::clientbound::config::RESOURCE_PACK_PUSH;

use crate::ClientPacket;
use crate::ser::NetworkWriteExt;
use pumpkin_util::version::JavaMinecraftVersion;

#[java_packet(RESOURCE_PACK_PUSH)]
pub struct CConfigAddResourcePack<'a> {
    pub uuid: &'a uuid::Uuid,
    pub url: &'a str,
    pub hash: &'a str, // max 40
    pub forced: bool,
    pub prompt_message: Option<TextComponent>,
}

impl<'a> CConfigAddResourcePack<'a> {
    #[must_use]
    pub const fn new(
        uuid: &'a uuid::Uuid,
        url: &'a str,
        hash: &'a str,
        forced: bool,
        prompt_message: Option<TextComponent>,
    ) -> Self {
        Self {
            uuid,
            url,
            hash,
            forced,
            prompt_message,
        }
    }
}

impl ClientPacket for CConfigAddResourcePack<'_> {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), crate::ser::WritingError> {
        if *version >= JavaMinecraftVersion::V_1_20_3 {
            write.write_uuid(self.uuid)?;
        }
        write.write_string(self.url)?;
        write.write_string_bounded(self.hash, 40)?;
        write.write_bool(self.forced)?;
        if let Some(prompt) = &self.prompt_message {
            write.write_bool(true)?;
            write.write_component(prompt, version)?;
        } else {
            write.write_bool(false)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_hash(hash: &str) -> Result<(), crate::ser::WritingError> {
        let uuid = uuid::Uuid::nil();
        let packet = CConfigAddResourcePack::new(&uuid, "", hash, false, None);
        packet.write_packet_data(Vec::new(), &JavaMinecraftVersion::V_26_2)
    }

    #[test]
    fn accepts_40_byte_hash() {
        assert!(write_hash(&"a".repeat(40)).is_ok());
    }

    #[test]
    fn rejects_41_byte_hash() {
        assert!(write_hash(&"a".repeat(41)).is_err());
    }
}
