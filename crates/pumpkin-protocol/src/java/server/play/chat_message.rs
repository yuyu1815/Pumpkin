use pumpkin_data::packet::serverbound::play::CHAT;
use pumpkin_macros::java_packet;
use pumpkin_util::version::JavaMinecraftVersion;

use crate::{
    ClientPacket, ServerPacket,
    codec::var_int::VarInt,
    ser::NetworkWriteExt,
    ser::{NetworkReadExt, NetworkReadSliceExt, ReadingError},
};

#[java_packet(CHAT)]
pub struct SChatMessage<'a> {
    pub message: &'a str,
    pub timestamp: i64,
    pub salt: i64,
    pub signature: Option<&'a [u8]>,
    pub message_count: VarInt,
    pub acknowledged: &'a [u8], // Bitset fixed 20 bits
    pub checksum: u8,           // 1.21.5 "fingerprint" checksum
}

impl<'a> ServerPacket<'a> for SChatMessage<'a> {
    fn read(read: &mut &'a [u8], version: &JavaMinecraftVersion) -> Result<Self, ReadingError> {
        let max_len = if version >= &JavaMinecraftVersion::V_1_11 {
            256
        } else {
            100
        };
        let message = read.get_str_bounded_borrowed(max_len)?;

        let mut timestamp = 0;
        let mut salt = 0;
        let mut signature = None;
        let mut message_count = VarInt(0);
        let mut acknowledged = &[][..];
        let mut checksum = 0;

        if version >= &JavaMinecraftVersion::V_1_19 {
            timestamp = read.get_i64_be()?;
            salt = read.get_i64_be()?;
            signature = read.get_option(|v| v.read_slice_borrowed(256))?;

            if version >= &JavaMinecraftVersion::V_1_19_3 {
                message_count = read.get_var_int()?;
                acknowledged = read.read_slice_borrowed(3)?;
            } else {
                let _signed_preview = read.get_u8()? != 0;
                if version >= &JavaMinecraftVersion::V_1_19_1 {
                    let previous_messages = read.get_var_int()?.0;
                    if !(0..=20).contains(&previous_messages) {
                        return Err(ReadingError::Message(format!(
                            "Invalid previous chat message count: {previous_messages}"
                        )));
                    }
                    for _ in 0..previous_messages {
                        let _sender = read.get_uuid()?;
                        let signature_len = read.get_var_int()?.0;
                        if !(0..=256).contains(&signature_len) {
                            return Err(ReadingError::Message(format!(
                                "Invalid previous chat signature length: {signature_len}"
                            )));
                        }
                        read.read_slice_borrowed(signature_len as usize)?;
                    }
                    let _last_rejected = read.get_option(|read| {
                        let _sender = read.get_uuid()?;
                        let signature_len = read.get_var_int()?.0;
                        if !(0..=256).contains(&signature_len) {
                            return Err(ReadingError::Message(format!(
                                "Invalid rejected chat signature length: {signature_len}"
                            )));
                        }
                        read.read_slice_borrowed(signature_len as usize)
                    })?;
                }
            }
        }

        if version >= &JavaMinecraftVersion::V_1_21_5 {
            checksum = read.get_u8()?;
        }

        Ok(Self {
            message,
            timestamp,
            salt,
            signature,
            message_count,
            acknowledged,
            checksum,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_legacy_last_seen_messages_completely() {
        let mut payload = vec![1, b'x'];
        payload.extend_from_slice(&[0; 8]);
        payload.extend_from_slice(&[0; 8]);
        payload.push(0); // absent signature
        payload.push(0); // signed preview
        payload.push(1); // one previous message
        payload.extend_from_slice(&[0; 16]); // sender UUID
        payload.extend_from_slice(&[0x80, 0x02]); // 256-byte signature
        payload.extend_from_slice(&[0; 256]);
        payload.push(0); // no rejected message

        let mut remaining = payload.as_slice();
        let packet = SChatMessage::read(&mut remaining, &JavaMinecraftVersion::V_1_19_1).unwrap();
        assert_eq!(packet.message, "x");
        assert!(remaining.is_empty(), "{} bytes remain", remaining.len());
    }
}

impl ClientPacket for SChatMessage<'_> {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), crate::ser::WritingError> {
        write.write_string(self.message)?;

        if version >= &JavaMinecraftVersion::V_1_19 {
            write.write_i64_be(self.timestamp)?;
            write.write_i64_be(self.salt)?;
            write.write_option(&self.signature, |p, v| p.write_slice(v))?;

            if version >= &JavaMinecraftVersion::V_1_19_3 {
                write.write_var_int(&self.message_count)?;
                write.write_slice(self.acknowledged)?;
            } else {
                // write_signed_preview dummy
                write.write_u8(0)?;
            }
        }

        if version >= &JavaMinecraftVersion::V_1_21_5 {
            write.write_u8(self.checksum)?;
        }

        Ok(())
    }
}
