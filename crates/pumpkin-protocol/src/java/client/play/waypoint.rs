use std::io::Write;

use crate::ser::NetworkWriteExt;
use crate::{ClientPacket, VarInt, WritingError};
use pumpkin_data::packet::clientbound::play::WAYPOINT;
use pumpkin_macros::java_packet;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::version::JavaMinecraftVersion;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum WaypointOperation {
    Track = 0,
    Untrack = 1,
    Update = 2,
}

#[derive(Clone, Debug)]
pub enum WaypointTarget {
    Position(BlockPos),
    Chunk { x: i32, z: i32 },
    Azimuth(f32),
    Empty,
}

#[derive(Clone, Debug)]
pub struct WaypointIcon<'a> {
    pub style: &'a str,
    /// Packed RGB (`0xRRGGBB`); `None` represents an absent color.
    pub color: Option<u32>,
}

#[derive(Clone, Debug)]
pub enum WaypointIdentifier<'a> {
    Uuid(Uuid),
    String(&'a str),
}

#[derive(Clone, Debug)]
pub struct TrackedWaypoint<'a> {
    pub identifier: WaypointIdentifier<'a>,
    pub icon: WaypointIcon<'a>,
    pub target: WaypointTarget,
}

impl TrackedWaypoint<'_> {
    #[must_use]
    pub const fn empty(identifier: Uuid) -> Self {
        Self {
            identifier: WaypointIdentifier::Uuid(identifier),
            icon: WaypointIcon {
                style: "minecraft:default",
                color: None,
            },
            target: WaypointTarget::Empty,
        }
    }

    #[must_use]
    pub const fn set_position(
        identifier: Uuid,
        icon: WaypointIcon<'_>,
        position: BlockPos,
    ) -> TrackedWaypoint<'_> {
        TrackedWaypoint {
            identifier: WaypointIdentifier::Uuid(identifier),
            icon,
            target: WaypointTarget::Position(position),
        }
    }
}

/// Syncs tracked waypoints (`ClientboundTrackedWaypointPacket`) to client.
#[java_packet(WAYPOINT)]
pub struct CWaypoint<'a> {
    pub operation: WaypointOperation,
    pub waypoint: TrackedWaypoint<'a>,
}

impl<'a> CWaypoint<'a> {
    #[must_use]
    pub const fn new(operation: WaypointOperation, waypoint: TrackedWaypoint<'a>) -> Self {
        Self {
            operation,
            waypoint,
        }
    }

    #[must_use]
    pub const fn remove(identifier: Uuid) -> Self {
        Self {
            operation: WaypointOperation::Untrack,
            waypoint: TrackedWaypoint::empty(identifier),
        }
    }

    #[must_use]
    pub const fn add_position(
        identifier: Uuid,
        icon: WaypointIcon<'a>,
        position: BlockPos,
    ) -> Self {
        Self {
            operation: WaypointOperation::Track,
            waypoint: TrackedWaypoint::set_position(identifier, icon, position),
        }
    }

    #[must_use]
    pub const fn update_position(
        identifier: Uuid,
        icon: WaypointIcon<'a>,
        position: BlockPos,
    ) -> Self {
        Self {
            operation: WaypointOperation::Update,
            waypoint: TrackedWaypoint::set_position(identifier, icon, position),
        }
    }
}

impl ClientPacket for CWaypoint<'_> {
    fn write_packet_data(
        &self,
        mut write: impl Write,
        _version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        write.write_var_int(&VarInt(self.operation as i32))?;

        match &self.waypoint.identifier {
            WaypointIdentifier::Uuid(uuid) => {
                write.write_bool(true)?;
                write.write_uuid(uuid)?;
            }
            WaypointIdentifier::String(identifier) => {
                write.write_bool(false)?;
                write.write_string(identifier)?;
            }
        }

        write.write_string(self.waypoint.icon.style)?;
        if let Some(color) = self.waypoint.icon.color {
            write.write_bool(true)?;
            write.write_all(&color.to_be_bytes()[1..])?;
        } else {
            write.write_bool(false)?;
        }

        match &self.waypoint.target {
            WaypointTarget::Empty => write.write_var_int(&VarInt(0))?,
            WaypointTarget::Position(pos) => {
                write.write_var_int(&VarInt(1))?;
                write.write_var_int(&VarInt(pos.0.x))?;
                write.write_var_int(&VarInt(pos.0.y))?;
                write.write_var_int(&VarInt(pos.0.z))?;
            }
            WaypointTarget::Chunk { x, z } => {
                write.write_var_int(&VarInt(2))?;
                write.write_var_int(&VarInt(*x))?;
                write.write_var_int(&VarInt(*z))?;
            }
            WaypointTarget::Azimuth(angle) => {
                write.write_var_int(&VarInt(3))?;
                write.write_f32_be(*angle)?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(packet: &CWaypoint<'_>) -> Vec<u8> {
        let mut bytes = Vec::new();
        packet
            .write_packet_data(&mut bytes, &JavaMinecraftVersion::V_1_21_2)
            .unwrap();
        bytes
    }

    fn icon(color: Option<u32>) -> WaypointIcon<'static> {
        WaypointIcon {
            style: "minecraft:default",
            color,
        }
    }

    #[test]
    fn uuid_identifier_required_icon_absent_rgb_and_vec3i_golden() {
        let packet = CWaypoint::add_position(Uuid::nil(), icon(None), BlockPos::new(1, -1, 2));
        assert_eq!(
            encode(&packet),
            [
                0x00, 0x01, // Track, UUID identifier
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, // UUID
                0x11, b'm', b'i', b'n', b'e', b'c', b'r', b'a', b'f', b't', b':', b'd', b'e', b'f',
                b'a', b'u', b'l', b't', 0x00, // absent RGB
                0x01, 0x01, 0xff, 0xff, 0xff, 0xff, 0x0f, 0x02, // Vec3i target
            ]
        );
    }

    #[test]
    fn string_identifier_and_present_rgb_golden() {
        let packet = CWaypoint::new(
            WaypointOperation::Update,
            TrackedWaypoint {
                identifier: WaypointIdentifier::String("home"),
                icon: icon(Some(0x12_3456)),
                target: WaypointTarget::Empty,
            },
        );
        assert_eq!(
            encode(&packet),
            [
                0x02, 0x00, 0x04, b'h', b'o', b'm', b'e', // Update, String identifier
                0x11, b'm', b'i', b'n', b'e', b'c', b'r', b'a', b'f', b't', b':', b'd', b'e', b'f',
                b'a', b'u', b'l', b't', 0x01, 0x00, 0x12, 0x34,
                0x56, // required Icon, present RGB
                0x00, // Empty target
            ]
        );
    }

    #[test]
    fn colored_icon_full_packet_uses_rgb_bytes() {
        let packet = CWaypoint::add_position(
            Uuid::nil(),
            icon(Some(0x12_3456)),
            BlockPos::new(1, -1, 2),
        );
        assert_eq!(
            crate::java::packet_encoder::serialize_packet(
                &packet,
                &JavaMinecraftVersion::V_26_2,
            )
            .unwrap()
            .as_ref(),
            [
                0x8a, 0x01, // 26.2 Waypoint packet ID
                0x00, 0x01, // Track, UUID identifier
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, // UUID
                0x11, b'm', b'i', b'n', b'e', b'c', b'r', b'a', b'f', b't', b':', b'd', b'e', b'f',
                b'a', b'u', b'l', b't', 0x01, 0x12, 0x34, 0x56, // Present RGB in R,G,B order
                0x01, 0x01, 0xff, 0xff, 0xff, 0xff, 0x0f, 0x02, // Vec3i target
            ]
        );
    }

    #[test]
    fn chunk_azimuth_and_untrack_tombstone_golden() {
        let chunk = CWaypoint::new(
            WaypointOperation::Track,
            TrackedWaypoint {
                identifier: WaypointIdentifier::String("x"),
                icon: icon(None),
                target: WaypointTarget::Chunk { x: -1, z: 2 },
            },
        );
        assert_eq!(
            encode(&chunk),
            [
                0x00, 0x00, 0x01, b'x', 0x11, b'm', b'i', b'n', b'e', b'c', b'r', b'a', b'f', b't',
                b':', b'd', b'e', b'f', b'a', b'u', b'l', b't', 0x00, 0x02, 0xff, 0xff, 0xff, 0xff,
                0x0f, 0x02,
            ]
        );

        let azimuth = CWaypoint::new(
            WaypointOperation::Update,
            TrackedWaypoint {
                identifier: WaypointIdentifier::Uuid(Uuid::nil()),
                icon: icon(None),
                target: WaypointTarget::Azimuth(1.0),
            },
        );
        let mut azimuth_golden = vec![
            0x02, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x11, b'm', b'i', b'n',
            b'e', b'c', b'r', b'a', b'f', b't', b':', b'd', b'e', b'f', b'a', b'u', b'l', b't',
            0x00, 0x03,
        ];
        azimuth_golden.extend_from_slice(&1.0_f32.to_be_bytes());
        assert_eq!(encode(&azimuth), azimuth_golden);

        let tombstone = CWaypoint::remove(Uuid::nil());
        assert_eq!(
            encode(&tombstone),
            [
                0x01, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x11, b'm', b'i', b'n',
                b'e', b'c', b'r', b'a', b'f', b't', b':', b'd', b'e', b'f', b'a', b'u', b'l', b't',
                0x00, 0x00,
            ]
        );
    }
}
