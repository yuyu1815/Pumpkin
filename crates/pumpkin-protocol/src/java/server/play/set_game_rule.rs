use pumpkin_data::packet::serverbound::play::SET_GAME_RULE;
use pumpkin_macros::java_packet;

use crate::{
    ServerPacket,
    codec::var_int::VarInt,
    ser::{NetworkReadExt, NetworkReadSliceExt, ReadingError},
};
use pumpkin_util::version::JavaMinecraftVersion;

pub struct GameRuleEntry<'a> {
    pub game_rule_key: &'a str,
    pub value: &'a str,
}

#[java_packet(SET_GAME_RULE)]
pub struct SSetGameRule<'a> {
    pub entries: Vec<GameRuleEntry<'a>>,
}

const MIN_GAME_RULE_ENTRY_WIRE_BYTES: usize = 2;

impl<'a> ServerPacket<'a> for SSetGameRule<'a> {
    fn read(bytebuf: &mut &'a [u8], _version: &JavaMinecraftVersion) -> Result<Self, ReadingError> {
        let count = usize::try_from(bytebuf.get_var_int()?.0)
            .map_err(|_| ReadingError::Message("Negative game rule entry count".into()))?;
        let remaining = (*bytebuf).len();
        if count > remaining / MIN_GAME_RULE_ENTRY_WIRE_BYTES {
            return Err(ReadingError::TooLarge(format!(
                "game rule entry count {count} cannot fit in {remaining} remaining bytes"
            )));
        }
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let game_rule_key = bytebuf.get_str_borrowed()?;
            let value = bytebuf.get_str_borrowed()?;
            entries.push(GameRuleEntry {
                game_rule_key,
                value,
            });
        }
        Ok(Self { entries })
    }
}

impl crate::ClientPacket for SSetGameRule<'_> {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        _version: &JavaMinecraftVersion,
    ) -> Result<(), crate::ser::WritingError> {
        use crate::ser::NetworkWriteExt;
        write.write_var_int(&VarInt(self.entries.len() as i32))?;
        for entry in &self.entries {
            write.write_string(entry.game_rule_key)?;
            write.write_string(entry.value)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientPacket, VarInt, ser::NetworkWriteExt};

    #[test]
    fn rejects_negative_game_rule_entry_count() {
        let mut bytes = Vec::new();
        bytes.write_var_int(&VarInt(-1)).expect("count");
        let result = SSetGameRule::read(&mut bytes.as_slice(), &JavaMinecraftVersion::V_1_21_5);

        assert!(matches!(result, Err(ReadingError::Message(_))));
    }

    #[test]
    fn rejects_game_rule_count_that_cannot_fit_payload() {
        let mut bytes = Vec::new();
        bytes.write_var_int(&VarInt(i32::MAX)).expect("count");
        let result = SSetGameRule::read(&mut bytes.as_slice(), &JavaMinecraftVersion::V_1_21_5);

        assert!(matches!(result, Err(ReadingError::TooLarge(_))));
    }

    #[test]
    fn accepts_game_rule_entries_with_wire_fit_count() {
        let packet = SSetGameRule {
            entries: vec![GameRuleEntry {
                game_rule_key: "doDaylightCycle",
                value: "true",
            }],
        };
        let version = JavaMinecraftVersion::V_1_21_5;
        let mut bytes = Vec::new();
        packet
            .write_packet_data(&mut bytes, &version)
            .expect("encode game rule");

        let decoded =
            SSetGameRule::read(&mut bytes.as_slice(), &version).expect("wire-fit game rule");
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(
            decoded.entries[0].game_rule_key,
            packet.entries[0].game_rule_key
        );
        assert_eq!(decoded.entries[0].value, packet.entries[0].value);
    }
}
