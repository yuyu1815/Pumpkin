use pumpkin_data::packet::serverbound::config::CLIENT_INFORMATION;
use pumpkin_macros::java_packet;

use crate::VarInt;

use crate::{
    ServerPacket,
    ser::{NetworkReadExt, NetworkReadSliceExt, ReadingError},
};
use pumpkin_util::version::JavaMinecraftVersion;

/// Sent by the client to inform the server about its local settings
#[java_packet(CLIENT_INFORMATION)]
pub struct SClientInformationConfig<'a> {
    /// The language code used by the client (e.g., "`en_us`")
    pub locale: &'a str,
    /// The maximum number of chunks the client renders
    pub view_distance: i8,
    /// Visibility of chat messages (0: Enabled, 1: Commands Only, 2: Hidden)
    pub chat_mode: VarInt,
    /// Whether the client wants chat colors/formatting rendered
    pub chat_colors: bool,
    /// Bitmask representing displayed skin parts (e.g., cape, jacket, sleeves)
    pub skin_parts: u8,
    /// The player's dominant hand (0: Left, 1: Right)
    pub main_hand: VarInt,
    /// Whether the client wants text filtering (e.g., for profanity) enabled
    pub text_filtering: bool,
    /// Whether the player should appear in the server's online player list
    pub server_listing: bool,
    /// Particle display setting (0: All, 1: Decreased, 2: Minimal), added in 26.2
    pub particle_status: u8,
}

impl<'a> ServerPacket<'a> for SClientInformationConfig<'a> {
    fn read(bytebuf: &mut &'a [u8], version: &JavaMinecraftVersion) -> Result<Self, ReadingError> {
        let locale = bytebuf.get_str_borrowed()?;
        let view_distance = bytebuf.get_i8()?;
        let chat_mode = bytebuf.get_var_int()?;
        let chat_colors = bytebuf.get_bool()?;
        let skin_parts = bytebuf.get_u8()?;
        let main_hand = if version >= &JavaMinecraftVersion::V_1_9 {
            bytebuf.get_var_int()?
        } else {
            VarInt(1)
        };
        let text_filtering = if version >= &JavaMinecraftVersion::V_1_17 {
            bytebuf.get_bool()?
        } else {
            false
        };
        let server_listing = if version >= &JavaMinecraftVersion::V_1_18 {
            bytebuf.get_bool()?
        } else {
            true
        };
        let particle_status = if version >= &JavaMinecraftVersion::V_26_2 {
            let status = bytebuf.get_u8()?;
            if status > 2 {
                return Err(ReadingError::Message(format!(
                    "Invalid particle status: {status}"
                )));
            }
            status
        } else {
            0
        };

        Ok(Self {
            locale,
            view_distance,
            chat_mode,
            chat_colors,
            skin_parts,
            main_hand,
            text_filtering,
            server_listing,
            particle_status,
        })
    }
}

impl crate::ClientPacket for SClientInformationConfig<'_> {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), crate::ser::WritingError> {
        use crate::ser::NetworkWriteExt;
        write.write_string(self.locale)?;
        write.write_i8(self.view_distance)?;
        write.write_var_int(&self.chat_mode)?;
        write.write_bool(self.chat_colors)?;
        write.write_u8(self.skin_parts)?;
        if version >= &JavaMinecraftVersion::V_1_9 {
            write.write_var_int(&self.main_hand)?;
        }
        if version >= &JavaMinecraftVersion::V_1_17 {
            write.write_bool(self.text_filtering)?;
        }
        if version >= &JavaMinecraftVersion::V_1_18 {
            write.write_bool(self.server_listing)?;
        }
        if version >= &JavaMinecraftVersion::V_26_2 {
            if self.particle_status > 2 {
                return Err(crate::ser::WritingError::Message(format!(
                    "Invalid particle status: {}",
                    self.particle_status
                )));
            }
            write.write_u8(self.particle_status)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientPacket;

    #[test]
    fn client_information_26_2_particle_status_roundtrips_in_both_states() {
        let version = JavaMinecraftVersion::V_26_2;
        let config = SClientInformationConfig {
            locale: "en_us",
            view_distance: 8,
            chat_mode: VarInt(0),
            chat_colors: true,
            skin_parts: 0x7f,
            main_hand: VarInt(1),
            text_filtering: false,
            server_listing: true,
            particle_status: 2,
        };
        let play = crate::java::server::play::SClientInformationPlay {
            locale: config.locale,
            view_distance: config.view_distance,
            chat_mode: config.chat_mode,
            chat_colors: config.chat_colors,
            skin_parts: config.skin_parts,
            main_hand: config.main_hand,
            text_filtering: config.text_filtering,
            server_listing: config.server_listing,
            particle_status: config.particle_status,
        };
        for (encoded, read) in [
            (config.serialize_packet(&version).unwrap(), 0),
            (play.serialize_packet(&version).unwrap(), 1),
        ] {
            let payload = &encoded[1..];
            let mut remaining = payload;
            let status = if read == 0 {
                SClientInformationConfig::read(&mut remaining, &version)
                    .unwrap()
                    .particle_status
            } else {
                crate::java::server::play::SClientInformationPlay::read(&mut remaining, &version)
                    .unwrap()
                    .particle_status
            };
            assert_eq!(status, 2);
            assert!(remaining.is_empty());
        }
    }
}
