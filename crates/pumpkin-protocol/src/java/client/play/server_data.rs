use pumpkin_data::packet::clientbound::play::SERVER_DATA;
use pumpkin_macros::java_packet;
use pumpkin_util::text::TextComponent;

use crate::{ClientPacket, VarInt, ser::NetworkWriteExt};
use pumpkin_util::version::JavaMinecraftVersion;

#[java_packet(SERVER_DATA)]
pub struct CServerData<'a> {
    pub motd: &'a TextComponent,
    pub icon_base64: Option<&'a str>,
}

impl<'a> CServerData<'a> {
    #[must_use]
    pub const fn new(motd: &'a TextComponent, icon_base64: Option<&'a str>) -> Self {
        Self { motd, icon_base64 }
    }
}

impl ClientPacket for CServerData<'_> {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), crate::ser::WritingError> {
        if *version >= JavaMinecraftVersion::V_1_19_4 {
            write.write_component(self.motd, version)?;
            if let Some(icon) = self.icon_base64 {
                write.write_bool(true)?;
                let raw_b64 = icon.strip_prefix("data:image/png;base64,").unwrap_or(icon);
                if let Ok(bytes) =
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, raw_b64)
                {
                    write.write_var_int(&VarInt(bytes.len().try_into().map_err(|_| {
                        crate::ser::WritingError::Message(format!(
                            "{} isn't representable as a VarInt",
                            bytes.len()
                        ))
                    })?))?;
                    write.write_slice(&bytes)?;
                } else {
                    write.write_var_int(&VarInt(0))?;
                }
            } else {
                write.write_bool(false)?;
            }
        } else {
            write.write_bool(true)?;
            write.write_component(self.motd, version)?;
            if let Some(icon) = self.icon_base64 {
                write.write_bool(true)?;
                write.write_string(icon)?;
            } else {
                write.write_bool(false)?;
            }
            if *version < JavaMinecraftVersion::V_1_19_3 {
                write.write_bool(false)?;
            }
            if *version >= JavaMinecraftVersion::V_1_19_1
                && *version < JavaMinecraftVersion::V_1_20_5
            {
                write.write_bool(false)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn favicon_uses_byte_array_length_prefix() {
        let motd = TextComponent::text("hello");
        let packet = CServerData::new(&motd, Some("AQID"));
        let version = JavaMinecraftVersion::V_26_2;
        let mut bytes = Vec::new();
        packet.write_packet_data(&mut bytes, &version).unwrap();

        let mut expected = motd.encode_for_version(&version).to_vec();
        expected.extend_from_slice(&[1, 3, 1, 2, 3]);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn absent_favicon_writes_only_false_presence() {
        let motd = TextComponent::text("hello");
        let packet = CServerData::new(&motd, None);
        let version = JavaMinecraftVersion::V_26_2;
        let mut bytes = Vec::new();
        packet.write_packet_data(&mut bytes, &version).unwrap();

        let mut expected = motd.encode_for_version(&version).to_vec();
        expected.push(0);
        assert_eq!(bytes, expected);
    }
}
