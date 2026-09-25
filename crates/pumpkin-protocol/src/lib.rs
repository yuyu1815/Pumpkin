#![deny(clippy::unwrap_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::{
    io::{Error, Read, Write},
    pin::Pin,
    task::{Context, Poll},
};

use aes::cipher::BlockSizeUser;
use bytes::Bytes;
use codec::var_int::VarInt;
use hybrid_array::{Array, sizes::U1};
use pumpkin_util::{
    resource_location::ResourceLocation,
    text::{TextComponent, color::Color, style::Style},
    version::JavaMinecraftVersion,
};
use ser::{ReadingError, WritingError};

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub use crate::packet::{MultiVersionJavaPacket, Packet};

pub mod bedrock;
pub mod codec;
pub mod java;
pub mod packet;
#[cfg(feature = "query")]
pub mod query;
pub mod rcon;
pub mod ser;
pub mod serial;

pub const MAX_PACKET_SIZE: u64 = (1 << 21) - 1;
pub const MAX_PACKET_DATA_SIZE: usize = 8_388_608;

pub type FixedBitSet = Box<[u8]>;

/// Represents a compression threshold.
///
/// The threshold determines the minimum size of data that should be compressed.
/// Data smaller than the threshold will not be compressed.
pub type CompressionThreshold = usize;

/// Represents a compression level.
///
/// The level controls the amount of compression applied to the data.
/// Higher levels generally result in higher compression ratios, but also
/// increase CPU usage.
pub type CompressionLevel = u32;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ConnectionState {
    HandShake,
    Status,
    Login,
    Transfer,
    Config,
    Play,
}
pub struct InvalidConnectionState;

impl TryFrom<VarInt> for ConnectionState {
    type Error = InvalidConnectionState;

    fn try_from(value: VarInt) -> Result<Self, Self::Error> {
        let value = value.0;
        match value {
            1 => Ok(Self::Status),
            2 => Ok(Self::Login),
            3 => Ok(Self::Transfer),
            _ => Err(InvalidConnectionState),
        }
    }
}

#[derive(PartialEq, Eq, Clone)]
pub enum IdOr<T> {
    Id(u16),
    Value(T),
}

impl<T> IdOr<T> {
    pub fn read<R: ser::NetworkReadExt>(
        read: &mut R,
        read_value: impl FnOnce(&mut R) -> Result<T, ser::ReadingError>,
    ) -> Result<Self, ser::ReadingError> {
        let id = read.get_var_int()?.0;
        if id == 0 {
            Ok(Self::Value(read_value(read)?))
        } else {
            Ok(Self::Id((id - 1) as u16))
        }
    }

    pub fn write<W: ser::NetworkWriteExt>(
        &self,
        write: &mut W,
        write_value: impl FnOnce(&mut W, &T) -> Result<(), ser::WritingError>,
    ) -> Result<(), ser::WritingError> {
        match self {
            Self::Id(id) => write.write_var_int(&((*id as i32) + 1).into()),
            Self::Value(value) => {
                write.write_var_int(&0.into())?;
                write_value(write, value)
            }
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct SoundEvent {
    pub sound_name: ResourceLocation,
    pub range: Option<f32>,
}

type Aes128Cfb8Dec = cfb8::Decryptor<aes::Aes128>;

pub struct StreamDecryptor<R: AsyncRead + Unpin> {
    cipher: Aes128Cfb8Dec,
    read: R,
}

impl<R: AsyncRead + Unpin> StreamDecryptor<R> {
    pub const fn new(cipher: Aes128Cfb8Dec, stream: R) -> Self {
        Self {
            cipher,
            read: stream,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for StreamDecryptor<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let ref_self = self.get_mut();
        let read = Pin::new(&mut ref_self.read);
        let cipher = &mut ref_self.cipher;

        // Get the starting position
        let original_fill = buf.filled().len();
        // Read the raw data
        let internal_poll = read.poll_read(cx, buf);

        if matches!(internal_poll, Poll::Ready(Ok(()))) {
            // Decrypt the raw data in-place, note that our block size is 1 byte, so this is always safe
            for block in buf.filled_mut()[original_fill..].chunks_mut(Aes128Cfb8Dec::block_size()) {
                cipher.decrypt(block);
            }
        }

        internal_poll
    }
}

type Aes128Cfb8Enc = cfb8::Encryptor<aes::Aes128>;

///NOTE: This makes lots of small writes; make sure there is a buffer somewhere down the line
pub struct StreamEncryptor<W: AsyncWrite + Unpin> {
    cipher: Aes128Cfb8Enc,
    write: W,
    last_unwritten_encrypted_byte: Option<u8>,
}

impl<W: AsyncWrite + Unpin> StreamEncryptor<W> {
    pub fn new(cipher: Aes128Cfb8Enc, stream: W) -> Self {
        debug_assert_eq!(Aes128Cfb8Enc::block_size(), 1);
        Self {
            cipher,
            write: stream,
            last_unwritten_encrypted_byte: None,
        }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for StreamEncryptor<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        let ref_self = self.get_mut();
        let cipher = &mut ref_self.cipher;

        let mut total_written = 0;
        // Decrypt the raw data, note that our block size is 1 byte, so this is always safe
        for block in buf.chunks(Aes128Cfb8Enc::block_size()) {
            let mut out = [0u8];

            if let Some(out_to_use) = ref_self.last_unwritten_encrypted_byte {
                // This assumes that this `poll_write` is called on the same stream of bytes which I
                // think is a fair assumption, since thats an invariant for the TCP stream anyway.

                // This should never panic
                out[0] = out_to_use;
            } else {
                let out_block: &mut Array<u8, U1> = (&mut out[..])
                    .try_into()
                    .map_err(|_| Error::other("Output slice size does not match block size"))?;
                cipher
                    .encrypt_b2b(block, out_block)
                    .map_err(|_| Error::other("Encryption failed"))?;
            }

            let write = Pin::new(&mut ref_self.write);
            match write.poll_write(cx, &out) {
                Poll::Pending => {
                    ref_self.last_unwritten_encrypted_byte = Some(out[0]);
                    if total_written == 0 {
                        //If we didn't write anything, return pending
                        return Poll::Pending;
                    }
                    // Otherwise, we actually did write something
                    return Poll::Ready(Ok(total_written));
                }
                Poll::Ready(result) => {
                    ref_self.last_unwritten_encrypted_byte = None;
                    match result {
                        Ok(written) => total_written += written,
                        Err(err) => return Poll::Ready(Err(err)),
                    }
                }
            }
        }

        Poll::Ready(Ok(total_written))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let ref_self = self.get_mut();
        let write = Pin::new(&mut ref_self.write);
        write.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let ref_self = self.get_mut();
        let write = Pin::new(&mut ref_self.write);
        write.poll_shutdown(cx)
    }
}

pub struct RawPacket {
    pub id: i32,
    pub payload: Bytes,
}

pub trait ClientPacket: MultiVersionJavaPacket {
    fn write_packet_data(
        &self,
        write: impl Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError>;

    fn write_packet(
        &self,
        version: &JavaMinecraftVersion,
        write: impl Write,
    ) -> Result<(), WritingError> {
        crate::java::packet_encoder::write_packet(self, version, write)
    }

    fn serialize_packet(&self, version: &JavaMinecraftVersion) -> Result<Bytes, WritingError> {
        crate::java::packet_encoder::serialize_packet(self, version)
    }
}

pub trait ServerPacket<'a>: MultiVersionJavaPacket + Sized {
    fn read(read: &mut &'a [u8], version: &JavaMinecraftVersion) -> Result<Self, ReadingError>;
}

pub trait BClientPacket: Packet {
    fn write_packet(&self, writer: impl Write) -> Result<(), Error>;

    fn serialize_packet(&self) -> Result<Bytes, Error> {
        crate::bedrock::packet_encoder::serialize_packet(self)
    }
}

pub trait BServerPacket: Packet + Sized {
    fn read(read: impl Read) -> Result<Self, Error>;
}

/// Errors that can occur during packet encoding.
#[derive(Error, Debug)]
pub enum PacketEncodeError {
    #[error("Packet exceeds maximum length: {0}")]
    TooLong(usize),
    #[error("Compression failed {0}")]
    CompressionFailed(String),
    #[error("Writing packet failed: {0}")]
    Message(String),
}

#[derive(Error, Debug)]
pub enum PacketDecodeError {
    #[error("failed to decode packet ID")]
    DecodeID,
    #[error("packet exceeds maximum length")]
    TooLong,
    #[error("packet length is out of bounds")]
    OutOfBounds,
    #[error("malformed packet length VarInt: {0}")]
    MalformedLength(String),
    #[error("failed to decompress packet: {0}")]
    FailedDecompression(String), // Updated to include error details
    #[error("packet is uncompressed but greater than the threshold")]
    NotCompressed,
    #[error("the connection has closed")]
    ConnectionClosed,
    #[error("{0}")]
    Message(String),
}

impl From<ReadingError> for PacketDecodeError {
    fn from(value: ReadingError) -> Self {
        Self::FailedDecompression(value.to_string())
    }
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponse {
    /// The version on which the server is running. (Optional)
    pub version: Option<Version>,
    /// Information about currently connected players. (Optional)
    pub players: Option<Players>,
    /// The description displayed, also called MOTD (Message of the Day). (Optional)
    pub description: TextComponent,
    /// The icon displayed. (Optional)
    pub favicon: Option<String>,
    /// Whether players are forced to use secure chat.
    pub enforce_secure_chat: bool,
}
#[derive(Clone, serde::Serialize)]
pub struct Version {
    /// The name of the version (e.g. 1.21.4)
    pub name: String,
    /// The protocol version (e.g. 767)
    pub protocol: u32,
}

#[derive(Clone, serde::Serialize)]
pub struct Players {
    /// The maximum player count that the server allows.
    pub max: u32,
    /// The current online player count.
    pub online: u32,
    /// Information about currently connected players.
    /// Note: players can disable listing here.
    pub sample: Vec<Sample>,
}

#[derive(Clone, serde::Serialize)]
pub struct Sample {
    /// The player's name.
    pub name: String,
    /// The player's UUID.
    pub id: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Property {
    pub name: Box<str>,
    pub value: Box<str>,
    pub signature: Option<Box<str>>,
}

impl Property {
    pub fn read(read: &mut impl ser::NetworkReadExt) -> Result<Self, ser::ReadingError> {
        Ok(Self {
            name: read.get_str()?,
            value: read.get_str()?,
            signature: read.get_option(ser::NetworkReadExt::get_str)?,
        })
    }

    pub fn write(&self, write: &mut impl ser::NetworkWriteExt) -> Result<(), ser::WritingError> {
        write.write_string(&self.name)?;
        write.write_string(&self.value)?;
        write.write_option(&self.signature, |w, v| w.write_string(v))?;
        Ok(())
    }
}

pub struct KnownPack<'a> {
    pub namespace: &'a str,
    pub id: &'a str,
    pub version: &'a str,
}

impl KnownPack<'_> {
    pub fn write(&self, write: &mut impl ser::NetworkWriteExt) -> Result<(), ser::WritingError> {
        write.write_string(self.namespace)?;
        write.write_string(self.id)?;
        write.write_string(self.version)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NumberFormat {
    /// Show nothing.
    Blank,
    /// The styling to be used when formatting the score number.
    Styled(Style),
    /// The text to be used as a placeholder.
    Fixed(TextComponent),
}

impl NumberFormat {
    pub fn write(&self, write: &mut impl ser::NetworkWriteExt) -> Result<(), ser::WritingError> {
        match self {
            Self::Blank => write.write_var_int(&0.into()),
            Self::Styled(style) => {
                write.write_var_int(&1.into())?;
                write_style_nbt(style, write)
            }
            Self::Fixed(text) => {
                write.write_var_int(&2.into())?;
                write.write_slice(&text.encode())?;
                Ok(())
            }
        }
    }
}

/// Writes a Style using the unnamed NBT compound used by Minecraft's styled number format.
pub fn write_style_nbt(
    style: &Style,
    write: &mut impl ser::NetworkWriteExt,
) -> Result<(), WritingError> {
    let mut compound = pumpkin_nbt::compound::NbtCompound::new();
    if let Some(color) = &style.color {
        let color = match color {
            Color::Named(color) => color.to_string(),
            Color::Rgb(color) => format!("#{:02x}{:02x}{:02x}", color.red, color.green, color.blue),
            Color::Reset => {
                return Err(WritingError::Message(
                    "Style color `reset` is not supported by the Minecraft TextColor codec".into(),
                ));
            }
        };
        compound.put_string("color", color);
    }
    for (key, value) in [
        ("bold", style.bold),
        ("italic", style.italic),
        ("underlined", style.underlined),
        ("strikethrough", style.strikethrough),
        ("obfuscated", style.obfuscated),
    ] {
        if let Some(value) = value {
            compound.put_bool(key, value);
        }
    }
    if let Some(insertion) = &style.insertion {
        compound.put_string("insertion", insertion.clone());
    }
    if let Some(font) = &style.font {
        let (namespace, path) = font.split_once(':').unwrap_or(("minecraft", font));
        let valid_namespace = !namespace.is_empty()
            && namespace
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.-".contains(&c));
        let valid_path = !path.is_empty()
            && path
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_./-".contains(&c));
        if !valid_namespace || !valid_path {
            return Err(WritingError::Message(format!(
                "Invalid font resource identifier: {font}"
            )));
        }
        compound.put_string("font", font.clone());
    }
    if let Some(color) = style.shadow_color {
        let packed = (i32::from(color.alpha) << 24)
            | (i32::from(color.red) << 16)
            | (i32::from(color.green) << 8)
            | i32::from(color.blue);
        compound.put_int("shadow_color", packed);
    }
    if style.click_event.is_some() {
        return Err(WritingError::Message(
            "Style click_event is not supported by the styled number format writer".into(),
        ));
    }
    if style.hover_event.is_some() {
        return Err(WritingError::Message(
            "Style hover_event is not supported by the styled number format writer".into(),
        ));
    }
    let bytes = pumpkin_nbt::Nbt::from(compound).write_unnamed();
    write.write_slice(&bytes)
}

#[cfg(test)]
mod number_format_style_tests {
    use super::*;
    use pumpkin_util::text::color::NamedColor;

    #[test]
    fn styled_number_format_writes_empty_compound() {
        let mut bytes = Vec::new();
        NumberFormat::Styled(Style::default())
            .write(&mut bytes)
            .unwrap();
        assert_eq!(bytes, [1, 10, 0, 0]);
    }

    #[test]
    fn styled_number_format_writes_rgb_color_as_hex_string() {
        let style = Style {
            color: Some(Color::Rgb(pumpkin_util::text::color::RGBColor {
                red: 0x12,
                green: 0xab,
                blue: 0xef,
            })),
            ..Style::default()
        };
        let mut bytes = Vec::new();
        NumberFormat::Styled(style).write(&mut bytes).unwrap();
        assert!(
            bytes
                .windows(17)
                .any(|window| window == b"\x08\x00\x05color\x00\x07#12abef")
        );
    }

    #[test]
    fn styled_number_format_writes_boolean_flags_as_nbt_bytes() {
        for (key, value) in [
            ("bold", true),
            ("italic", false),
            ("underlined", true),
            ("strikethrough", false),
            ("obfuscated", true),
        ] {
            let style = match key {
                "bold" => Style {
                    bold: Some(value),
                    ..Style::default()
                },
                "italic" => Style {
                    italic: Some(value),
                    ..Style::default()
                },
                "underlined" => Style {
                    underlined: Some(value),
                    ..Style::default()
                },
                "strikethrough" => Style {
                    strikethrough: Some(value),
                    ..Style::default()
                },
                _ => Style {
                    obfuscated: Some(value),
                    ..Style::default()
                },
            };
            let mut bytes = Vec::new();
            NumberFormat::Styled(style).write(&mut bytes).unwrap();
            let entry = [
                b"\x01\x00".as_slice(),
                &[key.len() as u8],
                key.as_bytes(),
                &[u8::from(value)],
            ]
            .concat();
            assert!(
                bytes.windows(entry.len()).any(|window| window == entry),
                "{key}"
            );
        }
    }

    #[test]
    fn styled_number_format_rejects_unsupported_style_fields() {
        let cases = [
            Style {
                color: Some(Color::Reset),
                ..Style::default()
            },
            Style {
                click_event: Some(pumpkin_util::text::click::ClickEvent::OpenUrl {
                    url: "https://example.com".into(),
                }),
                ..Style::default()
            },
            Style {
                hover_event: Some(pumpkin_util::text::hover::HoverEvent::ShowText {
                    value: vec![],
                }),
                ..Style::default()
            },
        ];
        for style in cases {
            assert!(matches!(
                NumberFormat::Styled(style).write(&mut Vec::new()),
                Err(WritingError::Message(_))
            ));
        }
    }

    #[test]
    fn styled_number_format_writes_supported_style_fields() {
        let style = Style {
            color: Some(Color::Named(NamedColor::Red)),
            bold: Some(true),
            insertion: Some("insert".into()),
            font: Some("minecraft:uniform".into()),
            shadow_color: Some(pumpkin_util::text::color::ARGBColor {
                alpha: 0x80,
                red: 1,
                green: 2,
                blue: 3,
            }),
            ..Style::default()
        };
        let mut bytes = Vec::new();
        NumberFormat::Styled(style).write(&mut bytes).unwrap();
        assert_eq!(&bytes[..2], &[1, 10]);
        for entry in [
            b"\x08\x00\x05color\x00\x03red".as_slice(),
            b"\x01\x00\x04bold\x01".as_slice(),
            b"\x08\x00\x09insertion\x00\x06insert".as_slice(),
            b"\x08\x00\x04font\x00\x11minecraft:uniform".as_slice(),
            b"\x03\x00\x0cshadow_color\x80\x01\x02\x03".as_slice(),
        ] {
            assert!(bytes.windows(entry.len()).any(|window| window == entry));
        }
    }
}

/// For the first 8 values set means relative value while unset means absolute
#[derive(Debug, PartialEq, Eq, Hash)]
pub enum PositionFlag {
    X,
    Y,
    Z,
    YRot,
    XRot,
    DeltaX,
    DeltaY,
    DeltaZ,
    RotateDelta,
}

impl PositionFlag {
    const fn get_mask(&self) -> i32 {
        match self {
            Self::X => 1 << 0,
            Self::Y => 1 << 1,
            Self::Z => 1 << 2,
            Self::YRot => 1 << 3,
            Self::XRot => 1 << 4,
            Self::DeltaX => 1 << 5,
            Self::DeltaY => 1 << 6,
            Self::DeltaZ => 1 << 7,
            Self::RotateDelta => 1 << 8,
        }
    }

    #[must_use]
    pub fn get_bitfield(flags: &[Self]) -> i32 {
        flags.iter().fold(0, |acc, flag| acc | flag.get_mask())
    }

    #[must_use]
    pub fn from_bitfield(bits: i32) -> Vec<Self> {
        let all = [
            Self::X,
            Self::Y,
            Self::Z,
            Self::YRot,
            Self::XRot,
            Self::DeltaX,
            Self::DeltaY,
            Self::DeltaZ,
            Self::RotateDelta,
        ];
        all.into_iter()
            .filter(|flag| (bits & flag.get_mask()) != 0)
            .collect()
    }
}

#[derive(Clone, Debug)]
pub enum Label {
    BuiltIn(LinkType),
    TextComponent(Box<TextComponent>),
}

pub struct Link<'a> {
    pub is_built_in: bool,
    pub label: Label,
    pub url: &'a String,
}

impl<'a> Link<'a> {
    #[must_use]
    pub const fn new(label: Label, url: &'a String) -> Self {
        Self {
            is_built_in: match label {
                Label::BuiltIn(_) => true,
                Label::TextComponent(_) => false,
            },
            label,
            url,
        }
    }

    pub fn write(&self, write: &mut impl ser::NetworkWriteExt) -> Result<(), ser::WritingError> {
        match &self.label {
            Label::BuiltIn(link_type) => {
                write.write_bool(true)?;
                write.write_var_int(&(*link_type as i32).into())?;
            }
            Label::TextComponent(text_component) => {
                write.write_bool(false)?;
                write.write_slice(&text_component.encode())?;
            }
        }
        write.write_string(self.url)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(i32)]
pub enum LinkType {
    BugReport = 0,
    CommunityGuidelines = 1,
    Support = 2,
    Status = 3,
    Feedback = 4,
    Community = 5,
    Website = 6,
    Forums = 7,
    News = 8,
    Announcements = 9,
}
