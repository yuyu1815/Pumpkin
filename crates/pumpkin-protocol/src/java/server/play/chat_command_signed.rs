use pumpkin_data::packet::serverbound::play::CHAT_COMMAND_SIGNED;
use pumpkin_macros::java_packet;
use pumpkin_util::version::JavaMinecraftVersion;

use crate::{
    ClientPacket, ServerPacket,
    codec::var_int::VarInt,
    ser::{NetworkReadExt, NetworkReadSliceExt, NetworkWriteExt, ReadingError, WritingError},
};

pub struct ArgumentSignature<'a> {
    pub name: &'a str,
    pub signature: &'a [u8],
}

#[java_packet(CHAT_COMMAND_SIGNED)]
pub struct SChatCommandSigned<'a> {
    pub command: &'a str,
    pub timestamp: i64,
    pub salt: i64,
    pub argument_signatures: Vec<ArgumentSignature<'a>>,
    pub message_count: VarInt,
    pub acknowledged: &'a [u8],
    pub checksum: u8,
}

pub const MAX_ARGUMENT_SIGNATURES: usize = 8;
const MIN_ARGUMENT_SIGNATURE_WIRE_BYTES: usize = 1 + 256;

impl<'a> ServerPacket<'a> for SChatCommandSigned<'a> {
    fn read(read: &mut &'a [u8], version: &JavaMinecraftVersion) -> Result<Self, ReadingError> {
        let command = read.get_str_bounded_borrowed(256)?;
        let timestamp = read.get_i64_be()?;
        let salt = read.get_i64_be()?;
        let arg_count = usize::try_from(read.get_var_int()?.0)
            .map_err(|_| ReadingError::Message("Negative argument signature count".into()))?;
        if arg_count > MAX_ARGUMENT_SIGNATURES {
            return Err(ReadingError::TooLarge(format!(
                "argument signature count {arg_count} exceeds official maximum {MAX_ARGUMENT_SIGNATURES}"
            )));
        }
        let remaining = (*read).len();
        if arg_count > remaining / MIN_ARGUMENT_SIGNATURE_WIRE_BYTES {
            return Err(ReadingError::TooLarge(format!(
                "argument signature count {arg_count} cannot fit in {remaining} remaining bytes"
            )));
        }
        let mut argument_signatures = Vec::with_capacity(arg_count);
        for _ in 0..arg_count {
            let name = read.get_str_bounded_borrowed(16)?;
            let signature = read.read_slice_borrowed(256)?;
            argument_signatures.push(ArgumentSignature { name, signature });
        }
        let message_count = read.get_var_int()?;
        let acknowledged = read.read_slice_borrowed(3)?;
        let checksum = if *version >= JavaMinecraftVersion::V_1_21_5 {
            read.get_u8()?
        } else {
            0
        };

        Ok(Self {
            command,
            timestamp,
            salt,
            argument_signatures,
            message_count,
            acknowledged,
            checksum,
        })
    }
}

impl ClientPacket for SChatCommandSigned<'_> {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        write.write_string(self.command)?;
        write.write_i64_be(self.timestamp)?;
        write.write_i64_be(self.salt)?;
        if self.argument_signatures.len() > MAX_ARGUMENT_SIGNATURES {
            return Err(WritingError::Message(format!(
                "argument signature count {} exceeds official maximum {MAX_ARGUMENT_SIGNATURES}",
                self.argument_signatures.len()
            )));
        }
        write.write_var_int(&VarInt(self.argument_signatures.len() as i32))?;
        for arg in &self.argument_signatures {
            write.write_string(arg.name)?;
            write.write_slice(arg.signature)?;
        }
        write.write_var_int(&self.message_count)?;
        write.write_slice(self.acknowledged)?;
        if *version >= JavaMinecraftVersion::V_1_21_5 {
            write.write_u8(self.checksum)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientPacket, ser::NetworkWriteExt};

    fn signed_command_prefix(count: i32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.write_var_int(&VarInt(0)).expect("command");
        bytes.write_i64_be(0).expect("timestamp");
        bytes.write_i64_be(0).expect("salt");
        bytes.write_var_int(&VarInt(count)).expect("count");
        bytes
    }

    #[test]
    fn rejects_negative_signed_command_argument_count() {
        let bytes = signed_command_prefix(-1);
        let result =
            SChatCommandSigned::read(&mut bytes.as_slice(), &JavaMinecraftVersion::V_1_21_5);

        assert!(matches!(result, Err(ReadingError::Message(_))));
    }

    #[test]
    fn rejects_signed_command_count_that_cannot_fit_payload() {
        let bytes = signed_command_prefix(i32::MAX);
        let result =
            SChatCommandSigned::read(&mut bytes.as_slice(), &JavaMinecraftVersion::V_1_21_5);

        assert!(matches!(result, Err(ReadingError::TooLarge(_))));
    }

    #[test]
    fn rejects_signed_command_argument_count_above_official_maximum() {
        let signature = [7u8; 256];
        let mut bytes = signed_command_prefix((MAX_ARGUMENT_SIGNATURES + 1) as i32);
        for _ in 0..=MAX_ARGUMENT_SIGNATURES {
            bytes.write_string("target").expect("argument name");
            bytes.write_slice(&signature).expect("argument signature");
        }
        bytes.write_var_int(&VarInt(0)).expect("message count");
        bytes.extend_from_slice(&[0; 3]);
        bytes.push(0);

        let result =
            SChatCommandSigned::read(&mut bytes.as_slice(), &JavaMinecraftVersion::V_1_21_5);

        assert!(matches!(result, Err(ReadingError::TooLarge(_))));
    }

    #[test]
    fn accepts_signed_command_with_wire_fit_argument_signature() {
        let signature = [7u8; 256];
        let acknowledged = [0u8; 3];
        let packet = SChatCommandSigned {
            command: "say hello",
            timestamp: 1,
            salt: 2,
            argument_signatures: vec![ArgumentSignature {
                name: "target",
                signature: &signature,
            }],
            message_count: VarInt(0),
            acknowledged: &acknowledged,
            checksum: 0,
        };
        let version = JavaMinecraftVersion::V_1_21_5;
        let mut bytes = Vec::new();
        packet
            .write_packet_data(&mut bytes, &version)
            .expect("encode signed command");

        let decoded = SChatCommandSigned::read(&mut bytes.as_slice(), &version)
            .expect("wire-fit signed command");
        assert_eq!(decoded.command, packet.command);
        assert_eq!(decoded.argument_signatures.len(), 1);
        assert_eq!(decoded.argument_signatures[0].signature, &signature);
    }
}
