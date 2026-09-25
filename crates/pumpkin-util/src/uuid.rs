use uuid::Uuid;

/// Generates the UUID used by Minecraft for players in offline mode.
#[must_use]
pub fn offline_player_uuid(name: &str) -> Uuid {
    let digest = md5::compute([b"OfflinePlayer:".as_slice(), name.as_bytes()].concat());
    let mut bytes = digest.0;
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// Parses a UUID similar to the way that Java does, which makes it ideal for
/// keeping the same behaviors sometimes.
#[must_use]
pub fn parse_uuid_array(uuid: &str) -> Option<[i32; 4]> {
    // We can't directly use the uuid crate to parse UUIDs, as it parses them
    // in a different way from Java.

    if uuid.len() > 36 {
        // UUID string is too large.
        return None;
    }

    // Split by hyphen. (5 segments)
    let mut parts = uuid.split('-');
    let mut parsed_parts: [i64; 5] = [0; 5];

    for part in &mut parsed_parts {
        // If a part is empty, the parsing functions will error anyway - this is what we want.
        *part = i64::from_str_radix(parts.next()?, 16).ok()?;
    }

    if parts.next().is_some() {
        // UUIDs must have exactly 5 parts.
        return None;
    }

    let bits = [
        (parsed_parts[0] & 0xFFFFFFFF) << 32
            | (parsed_parts[1] & 0xFFFF) << 16
            | (parsed_parts[2] & 0xFFFF),
        (parsed_parts[3] & 0xFFFF) << 48 | (parsed_parts[4] & 0xFFFFFFFFFFFF),
    ];

    Some([
        (bits[0] >> 32) as i32,
        bits[0] as i32,
        (bits[1] >> 32) as i32,
        bits[1] as i32,
    ])
}

/// Parses a UUID similar to the way that Java does, which makes it ideal for
/// keeping the same behaviors sometimes.
#[must_use]
pub fn parse_uuid_vec(uuid: &str) -> Option<Vec<i32>> {
    parse_uuid_array(uuid).map(Vec::from)
}

/// Parses a UUID similar to the way that Java does, which makes it ideal for
/// keeping the same behaviors sometimes.
#[must_use]
pub fn parse_uuid(uuid: &str) -> Option<Uuid> {
    let [a, b, c, d] = parse_uuid_array(uuid)?;

    let uuid_int = (a as u32 as u128) << 96
        | (b as u32 as u128) << 64
        | (c as u32 as u128) << 32
        | (d as u32 as u128);

    Some(Uuid::from_u128(uuid_int))
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use uuid::Uuid;

    use crate::uuid::{parse_uuid, parse_uuid_array};

    #[test]
    fn parse_uuids() {
        assert_eq!(
            crate::uuid::offline_player_uuid("UuidBot"),
            Uuid::parse_str("5cd85936-ba17-3080-8970-d7782f16c9b5").unwrap()
        );
        assert_eq!(
            parse_uuid_array("3d569d3a-93ef-44a0-9f1c-f69db9d37a56"),
            Some([1029086522, -1813035872, -1625491811, -1177322922])
        );
        assert_eq!(
            parse_uuid_array("3d53a-f-40-c-f69db9d37a56"),
            Some([251194, 983104, 849565, -1177322922])
        );
        assert_eq!(parse_uuid_array("3d53a-f40-c-f69db9d37a56"), None);
        assert_eq!(
            parse_uuid_array("fffffffffffffff-0-0-0-0"),
            Some([-1, 0, 0, 0])
        );
        assert_eq!(parse_uuid_array("ffffffffffffffff-0-0-0-0"), None);
        assert_eq!(
            parse_uuid_array("+1-+2-+3-+4-+5"),
            Some([1, 131075, 262144, 5])
        );
        assert_eq!(
            parse_uuid_array("aaaaaaaaaaaaaaa-bbbbbbbbbbbbbb-c-d-e"),
            Some([-1431655766, -1145372660, 851968, 14])
        );
        assert_eq!(
            parse_uuid_array("aaaaaaaaaaaaaaa-bbbbbbbbbbbbbb-c--e"),
            None
        );

        assert_eq!(
            parse_uuid("3d569d3a-93ef-44a0-9f1c-f69db9d37a56"),
            Some(Uuid::from_str("3d569d3a-93ef-44a0-9f1c-f69db9d37a56").unwrap())
        );
        assert_eq!(
            parse_uuid("3d53a-f-40-c-f69db9d37a56"),
            Some(Uuid::from_str("0003d53a-000f-0040-000c-f69db9d37a56").unwrap())
        );
        assert_eq!(
            parse_uuid("+1-+2-+3-+4-+5"),
            Some(Uuid::from_str("00000001-0002-0003-0004-000000000005").unwrap())
        );
    }
}
