#!/usr/bin/env python3
"""Small, dependency-free protocol 776 status/login/reconfiguration harness.

This is deliberately not a Minecraft client. It validates framing and the
small set of packet structures it actually consumes; unsupported mapped packets
are surfaced as needs_review and are never treated as evidence of client
compatibility. Reconfiguration mode is opt-in and waits for a server-owned
Rust trigger; it never exposes or sends a public server command.
"""
from __future__ import annotations

import argparse
import json
import math
import socket
import struct
import sys
import uuid
import zlib
from dataclasses import dataclass
from pathlib import Path
from typing import Any

PROTOCOL = 776
_DIAGNOSTIC_FILE: Path | None = None
_DIAGNOSTIC_SEQUENCE = 0


def _write_diagnostic(event: str, **fields: Any) -> None:
    global _DIAGNOSTIC_SEQUENCE
    if _DIAGNOSTIC_FILE is None:
        return
    _DIAGNOSTIC_SEQUENCE += 1
    payload = {"sequence": _DIAGNOSTIC_SEQUENCE, "event": event, **fields}
    try:
        _DIAGNOSTIC_FILE.parent.mkdir(parents=True, exist_ok=True)
        with _DIAGNOSTIC_FILE.open("a", encoding="utf-8") as trace:
            trace.write(json.dumps(payload, sort_keys=True) + "\n")
    except OSError:
        # Diagnostics must not alter protocol behavior; stderr/exit still carry
        # the primary failure when the trace file cannot be written.
        pass


MAX_PACKET_SIZE = 2_097_152
MAX_PACKET_DATA_SIZE = 8_388_608
MAX_VARINT_BYTES = 5


class HarnessError(RuntimeError):
    """A protocol, timeout, or validation failure."""


@dataclass(frozen=True)
class Frame:
    packet_id: int
    payload: bytes
    framed_length: int
    compressed: bool


def encode_varint(value: int) -> bytes:
    """Encode a signed Java VarInt, rejecting values outside int32."""
    if not -(1 << 31) <= value < (1 << 31):
        raise ValueError(f"VarInt outside int32: {value}")
    unsigned = value & 0xFFFFFFFF
    out = bytearray()
    while True:
        byte = unsigned & 0x7F
        unsigned >>= 7
        if unsigned:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def decode_varint(data: bytes, offset: int = 0) -> tuple[int, int]:
    """Decode one Java VarInt with the same five-byte bound used by the server."""
    result = 0
    for index in range(MAX_VARINT_BYTES):
        position = offset + index
        if position >= len(data):
            raise HarnessError("truncated VarInt")
        byte = data[position]
        result |= (byte & 0x7F) << (7 * index)
        if not byte & 0x80:
            if result & (1 << 31):
                result -= 1 << 32
            return result, position + 1
    raise HarnessError("VarInt exceeds five bytes")


def read_exact(stream: socket.socket, size: int) -> bytes:
    if size < 0:
        raise HarnessError(f"negative read size: {size}")
    chunks = bytearray()
    while len(chunks) < size:
        try:
            chunk = stream.recv(size - len(chunks))
        except socket.timeout as exc:
            raise HarnessError(f"socket timeout while reading {size} bytes") from exc
        if not chunk:
            raise HarnessError("connection closed while reading a frame")
        chunks.extend(chunk)
    return bytes(chunks)


def read_varint_stream(stream: socket.socket) -> int:
    raw = bytearray()
    for _ in range(MAX_VARINT_BYTES):
        raw.extend(read_exact(stream, 1))
        if not raw[-1] & 0x80:
            return decode_varint(bytes(raw))[0]
    raise HarnessError("frame VarInt exceeds five bytes")


def _zlib_decode(data: bytes, expected_size: int) -> bytes:
    decoder = zlib.decompressobj()
    try:
        result = decoder.decompress(data, MAX_PACKET_DATA_SIZE + 1)
        if len(result) > MAX_PACKET_DATA_SIZE:
            raise HarnessError("decompressed packet exceeds 8 MiB limit")
        result += decoder.flush(MAX_PACKET_DATA_SIZE - len(result))
    except zlib.error as exc:
        raise HarnessError(f"invalid zlib packet: {exc}") from exc
    if len(result) > MAX_PACKET_DATA_SIZE:
        raise HarnessError("decompressed packet exceeds 8 MiB limit")
    if not decoder.eof or decoder.unused_data or decoder.unconsumed_tail:
        raise HarnessError("zlib packet has a truncated stream or trailing data")
    if len(result) != expected_size:
        raise HarnessError(
            f"decompressed length mismatch: declared {expected_size}, got {len(result)}"
        )
    return result


def read_frame(stream: socket.socket, compression_threshold: int | None) -> Frame:
    framed_length = read_varint_stream(stream)
    if not 0 <= framed_length <= MAX_PACKET_SIZE:
        raise HarnessError(f"frame length outside 0..{MAX_PACKET_SIZE}: {framed_length}")
    framed = read_exact(stream, framed_length)
    compressed = False
    if compression_threshold is not None:
        data_length, offset = decode_varint(framed)
        if data_length < 0 or data_length > MAX_PACKET_DATA_SIZE:
            raise HarnessError(f"uncompressed length outside limit: {data_length}")
        if data_length == 0:
            raw = framed[offset:]
            if len(raw) >= compression_threshold:
                raise HarnessError(
                    "server sent an uncompressed packet at or above compression threshold"
                )
        else:
            raw = _zlib_decode(framed[offset:], data_length)
            compressed = True
    else:
        raw = framed
    if not raw:
        raise HarnessError("empty packet data")
    packet_id, offset = decode_varint(raw)
    if packet_id < 0:
        raise HarnessError(f"negative packet id: {packet_id}")
    return Frame(packet_id, raw[offset:], framed_length, compressed)


def frame_packet(packet_id: int, payload: bytes, compression_threshold: int | None) -> bytes:
    raw = encode_varint(packet_id) + payload
    if len(raw) > MAX_PACKET_DATA_SIZE:
        raise HarnessError("outgoing packet exceeds 8 MiB data limit")
    if compression_threshold is None:
        body = raw
    elif len(raw) >= compression_threshold:
        body = encode_varint(len(raw)) + zlib.compress(raw, 4)
    else:
        body = encode_varint(0) + raw
    if len(body) > MAX_PACKET_SIZE:
        raise HarnessError("outgoing frame exceeds 2 MiB limit")
    return encode_varint(len(body)) + body


def write_packet(
    stream: socket.socket, packet_id: int, payload: bytes, compression_threshold: int | None
) -> None:
    try:
        stream.sendall(frame_packet(packet_id, payload, compression_threshold))
    except OSError as exc:
        raise HarnessError(f"socket write failed: {exc}") from exc


def put_string(value: str, max_bytes: int = 32767) -> bytes:
    encoded = value.encode("utf-8")
    if len(encoded) > max_bytes:
        raise HarnessError(f"string exceeds {max_bytes} bytes")
    return encode_varint(len(encoded)) + encoded


def get_string(data: bytes, offset: int = 0, max_bytes: int = 32767) -> tuple[str, int]:
    length, offset = decode_varint(data, offset)
    if length < 0 or length > max_bytes:
        raise HarnessError(f"string length outside 0..{max_bytes}: {length}")
    end = offset + length
    if end > len(data):
        raise HarnessError("truncated string")
    try:
        value = data[offset:end].decode("utf-8")
    except UnicodeDecodeError as exc:
        raise HarnessError("invalid UTF-8 string") from exc
    return value, end


def require_end(data: bytes, offset: int, context: str) -> None:
    if offset != len(data):
        raise HarnessError(f"{context} has {len(data) - offset} trailing bytes")


def _asset_packet_ids() -> dict[str, dict[str, dict[str, int]]]:
    path = Path(__file__).resolve().parents[2] / "assets" / "packet" / "26_2_packets.json"
    try:
        with path.open(encoding="utf-8") as source:
            return {
                state: {
                    direction: {
                        name: int(packet["protocol_id"])
                        for name, packet in packets.items()
                    }
                    for direction, packets in directions.items()
                }
                for state, directions in json.load(source).items()
            }
    except (OSError, ValueError, KeyError, TypeError) as exc:
        raise HarnessError(f"cannot load existing packet mapping {path}: {exc}") from exc


def packet_id(mapping: dict[str, dict[str, dict[str, int]]], state: str, direction: str, name: str) -> int:
    try:
        return mapping[state][direction][f"minecraft:{name}"]
    except KeyError as exc:
        raise HarnessError(f"packet mapping lacks {state}/{direction}/{name}") from exc


def verify_critical_mapping(mapping: dict[str, dict[str, dict[str, int]]]) -> None:
    expected = {
        ("handshake", "serverbound", "intention"): 0,
        ("status", "serverbound", "status_request"): 0,
        ("status", "serverbound", "ping_request"): 1,
        ("status", "clientbound", "status_response"): 0,
        ("status", "clientbound", "pong_response"): 1,
        ("login", "serverbound", "hello"): 0,
        ("login", "serverbound", "login_acknowledged"): 3,
        ("login", "clientbound", "login_compression"): 3,
        ("login", "clientbound", "login_finished"): 2,
        ("configuration", "serverbound", "client_information"): 0,
        ("configuration", "serverbound", "select_known_packs"): 7,
        ("configuration", "serverbound", "finish_configuration"): 3,
        ("configuration", "serverbound", "keep_alive"): 4,
        ("configuration", "serverbound", "pong"): 5,
        ("configuration", "clientbound", "select_known_packs"): 14,
        ("configuration", "clientbound", "finish_configuration"): 3,
        ("configuration", "clientbound", "keep_alive"): 4,
        ("configuration", "clientbound", "ping"): 5,
        ("play", "serverbound", "configuration_acknowledged"): 16,
        ("play", "clientbound", "start_configuration"): 118,
        ("play", "serverbound", "keep_alive"): 28,
        ("play", "serverbound", "pong"): 45,
        ("play", "serverbound", "accept_teleportation"): 0,
        ("play", "clientbound", "disconnect"): 32,
        ("play", "clientbound", "keep_alive"): 44,
        ("play", "clientbound", "ping"): 61,
        ("play", "clientbound", "login"): 49,
        ("play", "clientbound", "level_chunk_with_light"): 45,
        ("play", "clientbound", "player_position"): 72,
    }
    for key, expected_id in expected.items():
        actual = packet_id(mapping, *key)
        if actual != expected_id:
            raise HarnessError(f"mapping drift for {key}: expected {expected_id}, got {actual}")


def parse_login_success(payload: bytes) -> dict[str, Any]:
    if len(payload) < 16:
        raise HarnessError("login success is shorter than UUID")
    player_uuid = str(uuid.UUID(bytes=payload[:16]))
    username, offset = get_string(payload, 16, 16 * 4)
    property_count, offset = decode_varint(payload, offset)
    if not 0 <= property_count <= 1024:
        raise HarnessError(f"login property count outside 0..1024: {property_count}")
    for _ in range(property_count):
        _, offset = get_string(payload, offset)
        _, offset = get_string(payload, offset)
        if offset >= len(payload):
            raise HarnessError("truncated login property signature flag")
        has_signature = payload[offset] != 0
        offset += 1
        if has_signature:
            _, offset = get_string(payload, offset)
    if offset + 16 > len(payload):
        raise HarnessError("26.2 login success has no session UUID")
    session_id = str(uuid.UUID(bytes=payload[offset : offset + 16]))
    offset += 16
    require_end(payload, offset, "login success")
    return {"uuid": player_uuid, "username": username, "session_id": session_id}


def parse_status(payload: bytes) -> dict[str, Any]:
    text, offset = get_string(payload)
    require_end(payload, offset, "status response")
    try:
        status = json.loads(text)
    except json.JSONDecodeError as exc:
        raise HarnessError(f"status response is not JSON: {exc}") from exc
    if not isinstance(status, dict):
        raise HarnessError("status response JSON is not an object")
    version = status.get("version")
    if not isinstance(version, dict) or version.get("protocol") != PROTOCOL:
        raise HarnessError(f"status protocol is not {PROTOCOL}: {version!r}")
    return status


def parse_known_packs(payload: bytes) -> list[dict[str, str]]:
    """Decode the bounded Known Packs envelope without assigning registry meaning."""
    count, offset = decode_varint(payload)
    if count < 0 or count > 64:
        raise HarnessError(f"Known Packs count outside 0..64: {count}")
    packs: list[dict[str, str]] = []
    for _ in range(count):
        namespace, offset = get_string(payload, offset, 32767)
        pack_id, offset = get_string(payload, offset, 32767)
        version, offset = get_string(payload, offset, 32767)
        packs.append({"namespace": namespace, "id": pack_id, "version": version})
    require_end(payload, offset, "Known Packs")
    return packs


def encode_known_packs(packs: list[dict[str, str]]) -> bytes:
    if len(packs) > 64:
        raise HarnessError(f"Known Packs response count outside 0..64: {len(packs)}")
    payload = bytearray(encode_varint(len(packs)))
    for pack in packs:
        for key in ("namespace", "id", "version"):
            payload.extend(put_string(pack[key], 32767))
    return bytes(payload)


# These are safety bounds for this observer, not claims about the maximum
# cardinality accepted by the official client.  The packet schema supplies the
# field order/types; the harness only decodes enough to prove frame boundaries,
# counted-list bounds, and ordering.
MAX_REGISTRY_ENTRIES = 65_536
MAX_TAG_CATEGORIES = 64
MAX_TAGS_PER_CATEGORY = 8_192
MAX_TAG_ENTRIES = 65_536
MAX_ENABLED_FEATURES = 1_024


def _read_unnamed_nbt(data: bytes, offset: int, context: str) -> int:
    """Skip one 26.2 Java NBT value and return its end offset.

    Current registry fixtures contain both the protocol's unnamed root form and
    repository-generated compounds with an empty two-byte root name. Accept
    either wire envelope, preferring the bounded empty-name-root parse when
    both forms are structurally possible. NBT meaning remains intentionally
    uninspected.
    """
    if offset >= len(data):
        raise HarnessError(f"truncated {context} NBT root tag")
    tag_id = data[offset]
    if tag_id == 0:
        raise HarnessError(f"{context} NBT root cannot be TAG_End")

    candidates: list[int] = []
    try:
        candidates.append(_skip_nbt_payload(data, offset + 1, tag_id))
    except HarnessError:
        pass

    if tag_id != 0 and data[offset + 1 : offset + 3] == b"\x00\x00":
        try:
            root_name_length, payload_offset = _read_u16(data, offset + 1, f"{context} NBT root name length")
            payload_offset += root_name_length
            if payload_offset > len(data):
                raise HarnessError(f"truncated {context} NBT root name")
            candidates.append(_skip_nbt_payload(data, payload_offset, tag_id))
        except HarnessError:
            pass

    if candidates:
        # A named-root candidate consumes strictly more bytes for the
        # repository-generated form; an ordinary unnamed value has no valid
        # named-root parse and therefore keeps its sole candidate.
        return max(candidates)
    raise HarnessError(f"invalid {context} NBT envelope")


def parse_update_enabled_features(payload: bytes) -> dict[str, Any]:
    count, offset = _read_nonnegative_varint(
        payload, 0, "enabled feature count", MAX_ENABLED_FEATURES
    )
    features: list[str] = []
    seen: set[str] = set()
    for _ in range(count):
        feature, offset = get_string(payload, offset, 32767)
        if feature in seen:
            raise HarnessError(f"duplicate enabled feature: {feature}")
        seen.add(feature)
        features.append(feature)
    require_end(payload, offset, "Update Enabled Features")
    return {"feature_count": count, "features": features}


def parse_registry_data(payload: bytes) -> dict[str, Any]:
    """Decode the Registry Data minimum envelope, never registry semantics."""
    registry_id, offset = get_string(payload, 0, 32767)
    count, offset = _read_nonnegative_varint(
        payload, offset, "Registry Data entry count", MAX_REGISTRY_ENTRIES
    )
    entry_ids: list[str] = []
    seen: set[str] = set()
    entries_with_data = 0
    nbt_bytes = 0
    for _ in range(count):
        entry_id, offset = get_string(payload, offset, 32767)
        if entry_id in seen:
            raise HarnessError(f"duplicate Registry Data entry: {entry_id}")
        seen.add(entry_id)
        has_data, offset = _read_bool(payload, offset, "Registry Data entry data flag")
        if has_data:
            start = offset
            offset = _read_unnamed_nbt(payload, offset, "Registry Data entry")
            nbt_bytes += offset - start
            if nbt_bytes > MAX_PACKET_DATA_SIZE:
                raise HarnessError("Registry Data NBT exceeds 8 MiB")
            entries_with_data += 1
        entry_ids.append(entry_id)
    require_end(payload, offset, "Registry Data")
    return {
        "registry_id": registry_id,
        "entry_count": count,
        "entries_with_data": entries_with_data,
        "nbt_bytes": nbt_bytes,
        "entry_ids": entry_ids,
    }


def parse_update_tags(payload: bytes) -> dict[str, Any]:
    """Decode Update Tags counts/strings/IDs without assigning tag meaning."""
    category_count, offset = _read_nonnegative_varint(
        payload, 0, "Update Tags category count", MAX_TAG_CATEGORIES
    )
    categories: list[dict[str, Any]] = []
    category_names: set[str] = set()
    total_tags = 0
    total_ids = 0
    for _ in range(category_count):
        category, offset = get_string(payload, offset, 32767)
        if category in category_names:
            raise HarnessError(f"duplicate Update Tags category: {category}")
        category_names.add(category)
        tag_count, offset = _read_nonnegative_varint(
            payload, offset, f"Update Tags {category} tag count", MAX_TAGS_PER_CATEGORY
        )
        tags: list[dict[str, Any]] = []
        tag_names: set[str] = set()
        for _ in range(tag_count):
            tag_name, offset = get_string(payload, offset, 65_535)
            if tag_name in tag_names:
                raise HarnessError(f"duplicate Update Tags tag: {tag_name}")
            tag_names.add(tag_name)
            entry_count, offset = _read_nonnegative_varint(
                payload, offset, f"Update Tags {tag_name} entry count", MAX_TAG_ENTRIES
            )
            ids: list[int] = []
            for _ in range(entry_count):
                value, offset = decode_varint(payload, offset)
                if value < 0:
                    raise HarnessError(f"negative Update Tags entry id: {value}")
                ids.append(value)
            tags.append({"name": tag_name, "entry_count": entry_count, "ids": ids})
            total_ids += entry_count
            if total_ids > MAX_PACKET_DATA_SIZE:
                raise HarnessError("Update Tags entries exceed bounded total")
        categories.append({"name": category, "tag_count": tag_count, "tags": tags})
        total_tags += tag_count
    require_end(payload, offset, "Update Tags")
    return {
        "category_count": category_count,
        "tag_count": total_tags,
        "entry_id_count": total_ids,
        "categories": categories,
    }


def make_known_packs_response(requested: list[dict[str, str]], case: str) -> list[dict[str, str]]:
    """Build only wire-valid selection fixtures; server semantics remain observed."""
    if case == "empty":
        return []
    if case == "legal_subset":
        # An empty list is a legal proper subset for a one-pack request.
        return requested[:-1]
    if case == "unknown":
        return requested + [{"namespace": "example", "id": "unknown", "version": "1"}]
    if case == "exact":
        return list(requested)
    raise HarnessError(f"unsupported Known Packs response case: {case}")


def synthetic_resource_pack_ack_fixture(pack_uuid: uuid.UUID, result: int) -> bytes:
    """Encode a local-only status fixture; this never downloads a resource pack."""
    if result < 0:
        raise HarnessError(f"resource-pack result must be non-negative: {result}")
    return pack_uuid.bytes + encode_varint(result)


def parse_resource_pack_ack_fixture(payload: bytes) -> dict[str, Any]:
    """Validate the config response envelope, not client download/installation."""
    if len(payload) < 16:
        raise HarnessError("resource-pack ack fixture is shorter than UUID")
    pack_uuid = str(uuid.UUID(bytes=payload[:16]))
    result, offset = decode_varint(payload, 16)
    require_end(payload, offset, "resource-pack ack fixture")
    return {"uuid": pack_uuid, "result": result, "download_observed": False}


def parse_i64(payload: bytes, context: str) -> int:
    if len(payload) != 8:
        raise HarnessError(f"{context} must contain exactly 8 bytes")
    return struct.unpack(">q", payload)[0]


def parse_i32(payload: bytes, context: str) -> int:
    if len(payload) != 4:
        raise HarnessError(f"{context} must contain exactly 4 bytes")
    return struct.unpack(">i", payload)[0]


def make_handshake(next_state: int) -> bytes:
    return (
        encode_varint(PROTOCOL)
        + put_string("127.0.0.1", 255)
        + struct.pack(">H", 25565)
        + encode_varint(next_state)
    )


def make_client_information() -> bytes:
    # locale, view distance, chat mode, colors, skin parts, main hand,
    # text filtering, server listing. These fields are parsed by Pumpkin.
    # Keep the synthetic loopback probe bounded while still requesting chunks.
    return (
        put_string("en_us", 64)
        + struct.pack(">b", 2)
        + encode_varint(0)
        + b"\x01"
        + b"\x7f"
        + encode_varint(1)
        + b"\x00\x01"
    )


def run_status(host: str, port: int, timeout: float, mapping: dict[str, dict[str, dict[str, int]]]) -> dict[str, Any]:
    with socket.create_connection((host, port), timeout=timeout) as stream:
        stream.settimeout(timeout)
        write_packet(stream, packet_id(mapping, "handshake", "serverbound", "intention"), make_handshake(1), None)
        write_packet(stream, packet_id(mapping, "status", "serverbound", "status_request"), b"", None)
        status_frame = read_frame(stream, None)
        if status_frame.packet_id != packet_id(mapping, "status", "clientbound", "status_response"):
            raise HarnessError(f"expected status response packet, got id {status_frame.packet_id}")
        status = parse_status(status_frame.payload)
        nonce = 0x1122334455667788
        write_packet(
            stream,
            packet_id(mapping, "status", "serverbound", "ping_request"),
            struct.pack(">q", nonce),
            None,
        )
        pong = read_frame(stream, None)
        if pong.packet_id != packet_id(mapping, "status", "clientbound", "pong_response"):
            raise HarnessError(f"expected status pong packet, got id {pong.packet_id}")
        if parse_i64(pong.payload, "status pong") != nonce:
            raise HarnessError("status pong payload did not round-trip")
        return {
            "mode": "status",
            "protocol": PROTOCOL,
            "status_version": status["version"],
            "status_pong": "pass",
        }


def _read_i32(data: bytes, offset: int, context: str) -> tuple[int, int]:
    end = offset + 4
    if end > len(data):
        raise HarnessError(f"truncated {context}")
    return struct.unpack(">i", data[offset:end])[0], end


def _read_i16(data: bytes, offset: int, context: str) -> tuple[int, int]:
    end = offset + 2
    if end > len(data):
        raise HarnessError(f"truncated {context}")
    return struct.unpack(">h", data[offset:end])[0], end


def _read_i64(data: bytes, offset: int, context: str) -> tuple[int, int]:
    end = offset + 8
    if end > len(data):
        raise HarnessError(f"truncated {context}")
    return struct.unpack(">q", data[offset:end])[0], end


def _read_f64(data: bytes, offset: int, context: str) -> tuple[float, int]:
    end = offset + 8
    if end > len(data):
        raise HarnessError(f"truncated {context}")
    value = struct.unpack(">d", data[offset:end])[0]
    if not math.isfinite(value):
        raise HarnessError(f"non-finite {context}")
    return value, end


def _read_f32(data: bytes, offset: int, context: str) -> tuple[float, int]:
    end = offset + 4
    if end > len(data):
        raise HarnessError(f"truncated {context}")
    value = struct.unpack(">f", data[offset:end])[0]
    if not math.isfinite(value):        raise HarnessError(f"non-finite {context}")
    return value, end


def _read_bool(data: bytes, offset: int, context: str) -> tuple[bool, int]:
    if offset >= len(data) or data[offset] not in (0, 1):
        value = None if offset >= len(data) else data[offset]
        nearby = data[max(0, offset - 16): min(len(data), offset + 16)].hex()
        raise HarnessError(
            f"invalid {context} boolean at offset {offset}: {value!r}; nearby={nearby}"
        )
    return bool(data[offset]), offset + 1


def _read_nonnegative_varint(data: bytes, offset: int, context: str, maximum: int) -> tuple[int, int]:
    value, offset = decode_varint(data, offset)
    if value < 0 or value > maximum:
        raise HarnessError(f"{context} outside 0..{maximum}: {value}")
    return value, offset


def parse_join_game(payload: bytes) -> dict[str, Any]:
    """Validate the 26.2 Join Game envelope; registry data is not in this packet."""
    offset = 0
    entity_id, offset = _read_i32(payload, offset, "Join Game entity id")
    hardcore, offset = _read_bool(payload, offset, "Join Game hardcore")
    dimension_count, offset = _read_nonnegative_varint(payload, offset, "Join Game dimension count", 1024)
    dimensions = []
    for _ in range(dimension_count):
        name, offset = get_string(payload, offset, 32767)
        dimensions.append(name)
    max_players, offset = _read_nonnegative_varint(payload, offset, "Join Game max players", 1_000_000)
    view_distance, offset = _read_nonnegative_varint(payload, offset, "Join Game view distance", 1024)
    simulated_distance, offset = _read_nonnegative_varint(payload, offset, "Join Game simulated distance", 1024)
    reduced_debug_info, offset = _read_bool(payload, offset, "Join Game reduced debug info")
    enabled_respawn_screen, offset = _read_bool(payload, offset, "Join Game respawn screen")
    limited_crafting, offset = _read_bool(payload, offset, "Join Game limited crafting")
    dimension_id, offset = decode_varint(payload, offset)
    dimension_name, offset = get_string(payload, offset, 32767)
    hashed_seed, offset = _read_i64(payload, offset, "Join Game hashed seed")
    if offset >= len(payload):
        raise HarnessError("truncated Join Game game mode")
    game_mode = payload[offset]
    offset += 1
    if game_mode > 3:
        raise HarnessError(f"invalid Join Game game mode: {game_mode}")
    if offset >= len(payload):
        raise HarnessError("truncated Join Game previous game mode")
    previous_game_mode = struct.unpack(">b", payload[offset : offset + 1])[0]
    offset += 1
    debug_world, offset = _read_bool(payload, offset, "Join Game debug world")
    flat_world, offset = _read_bool(payload, offset, "Join Game flat world")
    has_death_position, offset = _read_bool(payload, offset, "Join Game death position")
    if has_death_position:
        death_dimension, offset = get_string(payload, offset, 32767)
        _, offset = _read_i64(payload, offset, "Join Game death block position")
    else:
        death_dimension = None
    portal_cooldown, offset = _read_nonnegative_varint(payload, offset, "Join Game portal cooldown", 1_000_000)
    sea_level, offset = _read_nonnegative_varint(payload, offset, "Join Game sea level", 1_000_000)
    online_mode, offset = _read_bool(payload, offset, "Join Game online mode")
    enforce_secure_chat, offset = _read_bool(payload, offset, "Join Game secure chat")
    require_end(payload, offset, "Join Game")
    return {
        "entity_id": entity_id,
        "hardcore": hardcore,
        "dimensions": dimensions,
        "max_players": max_players,
        "view_distance": view_distance,
        "simulated_distance": simulated_distance,
        "dimension_id": dimension_id,
        "dimension_name": dimension_name,
        "hashed_seed": hashed_seed,
        "game_mode": game_mode,
        "previous_game_mode": previous_game_mode,
        "debug_world": debug_world,
        "flat_world": flat_world,
        "death_dimension": death_dimension,
        "portal_cooldown": portal_cooldown,
        "sea_level": sea_level,
        "online_mode": online_mode,
        "enforce_secure_chat": enforce_secure_chat,
    }


def parse_player_position(payload: bytes) -> dict[str, Any]:
    offset = 0
    teleport_id, offset = decode_varint(payload, offset)
    if teleport_id < 0:
        raise HarnessError(f"negative player position teleport id: {teleport_id}")
    coordinates = []
    for name in ("x", "y", "z", "delta_x", "delta_y", "delta_z"):
        value, offset = _read_f64(payload, offset, f"player position {name}")
        coordinates.append(value)
    yaw, offset = _read_f32(payload, offset, "player position yaw")
    pitch, offset = _read_f32(payload, offset, "player position pitch")
    relatives, offset = _read_i32(payload, offset, "player position relatives")
    require_end(payload, offset, "player position")
    return {
        "teleport_id": teleport_id,
        "x": coordinates[0],
        "y": coordinates[1],
        "z": coordinates[2],
        "delta_x": coordinates[3],
        "delta_y": coordinates[4],
        "delta_z": coordinates[5],
        "yaw": yaw,
        "pitch": pitch,
        "relatives": relatives,
    }


def _read_bitset(data: bytes, offset: int, context: str) -> tuple[tuple[int, ...], int]:
    count, offset = _read_nonnegative_varint(data, offset, f"{context} word count", 64)
    words = []
    for _ in range(count):
        word, offset = _read_i64(data, offset, f"{context} word")
        words.append(word)
    return tuple(words), offset


def _read_light_arrays(data: bytes, offset: int, context: str) -> tuple[int, int]:
    count, offset = _read_nonnegative_varint(data, offset, f"{context} array count", 256)
    total_bytes = 0
    for _ in range(count):
        length, offset = _read_nonnegative_varint(data, offset, f"{context} array length", MAX_PACKET_DATA_SIZE)
        end = offset + length
        if end > len(data):
            raise HarnessError(f"truncated {context} array")
        total_bytes += length
        if total_bytes > MAX_PACKET_DATA_SIZE:
            raise HarnessError(f"{context} arrays exceed 8 MiB")
        offset = end
    return count, offset


def _read_u16(data: bytes, offset: int, context: str) -> tuple[int, int]:
    end = offset + 2
    if end > len(data):
        raise HarnessError(f"truncated {context}")
    return struct.unpack(">H", data[offset:end])[0], end


def _skip_nbt_payload(data: bytes, offset: int, tag_id: int, depth: int = 0) -> int:
    """Skip one standard NBT value with strict byte/depth bounds.

    This intentionally does not interpret registry/block-entity meaning. It
    only decodes enough of the wire envelope to continue to light data.
    """
    if depth > 64:
        raise HarnessError("NBT nesting exceeds 64 levels")
    fixed_sizes = {1: 1, 2: 2, 3: 4, 4: 8, 5: 4, 6: 8}
    if tag_id in fixed_sizes:
        end = offset + fixed_sizes[tag_id]
        if end > len(data):
            raise HarnessError("truncated NBT scalar")
        return end
    if tag_id == 7:
        length, offset = _read_i32(data, offset, "NBT byte-array length")
        if length < 0 or length > MAX_PACKET_DATA_SIZE:
            raise HarnessError(f"NBT byte-array length outside limit: {length}")
        end = offset + length
        if end > len(data):
            raise HarnessError("truncated NBT byte array")
        return end
    if tag_id == 8:
        length, offset = _read_u16(data, offset, "NBT string length")
        end = offset + length
        if end > len(data):
            raise HarnessError("truncated NBT string")
        return end
    if tag_id == 9:
        if offset >= len(data):
            raise HarnessError("truncated NBT list type")
        element_type = data[offset]
        offset += 1
        length, offset = _read_i32(data, offset, "NBT list length")
        if length < 0 or length > MAX_PACKET_DATA_SIZE:
            raise HarnessError(f"NBT list length outside limit: {length}")
        for _ in range(length):
            offset = _skip_nbt_payload(data, offset, element_type, depth + 1)
        return offset
    if tag_id == 10:
        while True:
            if offset >= len(data):
                raise HarnessError("truncated NBT compound tag")
            child_type = data[offset]
            offset += 1
            if child_type == 0:
                return offset
            name_length, offset = _read_u16(data, offset, "NBT compound name length")
            end = offset + name_length
            if end > len(data):
                raise HarnessError("truncated NBT compound name")
            offset = _skip_nbt_payload(data, end, child_type, depth + 1)
    if tag_id == 11:
        length, offset = _read_i32(data, offset, "NBT int-array length")
        if length < 0 or length > MAX_PACKET_DATA_SIZE // 4:
            raise HarnessError(f"NBT int-array length outside limit: {length}")
        end = offset + length * 4
        if end > len(data):
            raise HarnessError("truncated NBT int array")
        return end
    if tag_id == 12:
        length, offset = _read_i32(data, offset, "NBT long-array length")
        if length < 0 or length > MAX_PACKET_DATA_SIZE // 8:
            raise HarnessError(f"NBT long-array length outside limit: {length}")
        end = offset + length * 8
        if end > len(data):
            raise HarnessError("truncated NBT long array")
        return end
    raise HarnessError(f"unknown NBT tag id {tag_id}")


def _skip_nbt(data: bytes, offset: int) -> int:
    if offset >= len(data):
        raise HarnessError("truncated NBT root tag")
    tag_id = data[offset]
    offset += 1
    if tag_id == 0:
        return offset
    name_length, offset = _read_u16(data, offset, "NBT root name length")
    end = offset + name_length
    if end > len(data):
        raise HarnessError("truncated NBT root name")
    return _skip_nbt_payload(data, end, tag_id)


def parse_first_chunk(payload: bytes) -> dict[str, Any]:
    """Validate the known 26.2 chunk/light envelope, not section semantics."""
    offset = 0
    chunk_x, offset = _read_i32(payload, offset, "chunk x")
    chunk_z, offset = _read_i32(payload, offset, "chunk z")
    heightmap_count, offset = _read_nonnegative_varint(payload, offset, "heightmap count", 16)
    if heightmap_count != 3:
        raise HarnessError(f"26.2 first chunk must contain 3 heightmaps, got {heightmap_count}")
    heightmap_keys = []
    for _ in range(heightmap_count):
        key, offset = _read_nonnegative_varint(payload, offset, "heightmap key", 32)
        length, offset = _read_nonnegative_varint(payload, offset, "heightmap length", 128)
        if length != 37:
            raise HarnessError(f"26.2 heightmap {key} must contain 37 longs, got {length}")
        end = offset + length * 8
        if end > len(payload):
            raise HarnessError("truncated heightmap values")
        offset = end
        heightmap_keys.append(key)
    if set(heightmap_keys) != {1, 4, 5}:
        raise HarnessError(f"unexpected 26.2 heightmap keys: {heightmap_keys}")

    section_data_length, offset = _read_nonnegative_varint(payload, offset, "chunk section data length", MAX_PACKET_DATA_SIZE)
    section_end = offset + section_data_length
    if section_end > len(payload):
        raise HarnessError("truncated opaque chunk section data")
    offset = section_end
    block_entity_count, offset = _read_nonnegative_varint(payload, offset, "chunk block entity count", 4096)
    for _ in range(block_entity_count):
        if offset >= len(payload):
            raise HarnessError("truncated chunk block entity packed xz")
        offset += 1
        _, offset = _read_i16(payload, offset, "chunk block entity y")
        _, offset = decode_varint(payload, offset)
        # Protocol 1.20.2+ writes NBT without the legacy root-name field.
        # Parse the unnamed form here so a following zero-valued light mask
        # cannot be mistaken for a legacy root name.
        if offset >= len(payload):
            raise HarnessError("truncated chunk block entity NBT root tag")
        tag_id = payload[offset]
        if tag_id == 0:
            raise HarnessError("chunk block entity NBT root cannot be TAG_End")
        offset = _skip_nbt_payload(payload, offset + 1, tag_id)

    sky_mask, offset = _read_bitset(payload, offset, "sky light mask")
    block_mask, offset = _read_bitset(payload, offset, "block light mask")
    empty_sky_mask, offset = _read_bitset(payload, offset, "empty sky light mask")
    empty_block_mask, offset = _read_bitset(payload, offset, "empty block light mask")
    sky_arrays, offset = _read_light_arrays(payload, offset, "sky light")
    block_arrays, offset = _read_light_arrays(payload, offset, "block light")
    if offset != len(payload):
        raise HarnessError(
            f"first chunk has {len(payload) - offset} trailing bytes "
            f"(offset={offset}, total={len(payload)}, chunk=({chunk_x},{chunk_z}), "
            f"section_data_length={section_data_length}, block_entities={block_entity_count}, "
            f"sky_masks={len(sky_mask)}, block_masks={len(block_mask)}, "
            f"sky_arrays={sky_arrays}, block_arrays={block_arrays})"
        )
    return {
        "chunk_x": chunk_x,
        "chunk_z": chunk_z,
        "heightmaps": heightmap_keys,
        "section_data_bytes": section_data_length,
        "block_entities": block_entity_count,
        "sky_light_mask_words": len(sky_mask),
        "block_light_mask_words": len(block_mask),
        "empty_sky_light_mask_words": len(empty_sky_mask),
        "empty_block_light_mask_words": len(empty_block_mask),
        "sky_light_arrays": sky_arrays,
        "block_light_arrays": block_arrays,
    }


@dataclass
class StrictPlayTracker:
    ids: dict[str, int]
    known_ids: set[int]
    join_game: dict[str, Any] | None = None
    position: dict[str, Any] | None = None
    position_syncs: int = 0
    teleport_confirms: list[int] | None = None
    first_chunk: dict[str, Any] | None = None
    keepalives_replied: int = 0
    opaque_packets: int = 0

    def __post_init__(self) -> None:
        if self.teleport_confirms is None:
            self.teleport_confirms = []

    def accept(self, frame: Frame) -> tuple[str, int | None]:
        if frame.packet_id not in self.known_ids:
            raise HarnessError(f"unknown 26.2 play clientbound packet id {frame.packet_id}")
        if frame.packet_id == self.ids["disconnect"]:
            reason, offset = get_string(frame.payload)
            require_end(frame.payload, offset, "play disconnect")
            raise HarnessError(f"server play disconnect: {reason}")
        if frame.packet_id == self.ids["login"]:
            if self.join_game is not None:
                raise HarnessError("invalid play state: duplicate Join Game")
            self.join_game = parse_join_game(frame.payload)
            return "join_game", None
        if frame.packet_id == self.ids["player_position"]:
            if self.join_game is None:
                raise HarnessError("invalid play state: position sync before Join Game")
            parsed = parse_player_position(frame.payload)
            self.position = parsed
            self.position_syncs += 1
            return "teleport_confirm", parsed["teleport_id"]
        if frame.packet_id == self.ids["level_chunk_with_light"]:
            if self.join_game is None:
                raise HarnessError("invalid play state: chunk before Join Game")
            if self.first_chunk is not None:
                self.opaque_packets += 1
                return "opaque", None
            self.first_chunk = parse_first_chunk(frame.payload)
            return "first_chunk", None
        if frame.packet_id == self.ids["keep_alive"]:
            if self.join_game is None:
                raise HarnessError("invalid play state: keep-alive before Join Game")
            return "keep_alive", parse_i64(frame.payload, "play keep-alive")
        if frame.packet_id == self.ids["ping"]:
            if self.join_game is None:
                raise HarnessError("invalid play state: ping before Join Game")
            return "ping", parse_i32(frame.payload, "play ping")
        if self.join_game is None:
            raise HarnessError("invalid play state: opaque packet before Join Game")
        self.opaque_packets += 1
        return "opaque", None

    def record_teleport_confirmation(self, teleport_id: int) -> None:
        if self.join_game is None or self.position_syncs == 0:
            raise HarnessError("invalid state while confirming teleport")
        self.teleport_confirms.append(teleport_id)

    def record_keepalive_reply(self) -> None:
        if self.join_game is None:
            raise HarnessError("invalid state while replying to keep-alive")
        self.keepalives_replied += 1

    def require_complete(self) -> None:
        missing = []
        if self.join_game is None:
            missing.append("join_game")
        if self.position_syncs == 0:
            missing.append("position_sync")
        if not self.teleport_confirms:
            missing.append("teleport_confirm")
        if self.first_chunk is None:
            missing.append("first_chunk")
        if self.keepalives_replied == 0:
            missing.append("keepalive_reply")
        if missing:
            raise HarnessError(f"strict first-playable requirements missing: {', '.join(missing)}")


def run_login(
    host: str,
    port: int,
    timeout: float,
    username: str,
    wait_for_keepalive: bool,
    mapping: dict[str, dict[str, dict[str, int]]],
) -> dict[str, Any]:
    if not 1 <= len(username) <= 16 or not username.isascii() or not username.replace("_", "").isalnum():
        raise HarnessError("username must be 1..16 ASCII alphanumeric/underscore characters")
    clientbound = {name: packet_id(mapping, "configuration", "clientbound", name) for name in (
        "disconnect", "keep_alive", "ping", "select_known_packs", "finish_configuration",
        "update_enabled_features", "registry_data", "update_tags",
    )}
    serverbound_config = {name: packet_id(mapping, "configuration", "serverbound", name) for name in (
        "client_information", "select_known_packs", "finish_configuration", "keep_alive", "pong"
    )}
    with socket.create_connection((host, port), timeout=timeout) as stream:
        stream.settimeout(timeout)
        write_packet(stream, packet_id(mapping, "handshake", "serverbound", "intention"), make_handshake(2), None)
        login_start = put_string(username, 16 * 4) + uuid.uuid4().bytes
        write_packet(stream, packet_id(mapping, "login", "serverbound", "hello"), login_start, None)

        compression: int | None = None
        login_success: dict[str, Any] | None = None
        login_packets = 0
        while login_success is None:
            frame = read_frame(stream, compression)
            login_packets += 1
            if frame.packet_id == packet_id(mapping, "login", "clientbound", "login_compression"):
                threshold, offset = decode_varint(frame.payload)
                require_end(frame.payload, offset, "login compression")
                if threshold < 0 or threshold > MAX_PACKET_DATA_SIZE:
                    raise HarnessError(f"invalid compression threshold: {threshold}")
                compression = threshold
            elif frame.packet_id == packet_id(mapping, "login", "clientbound", "login_finished"):
                login_success = parse_login_success(frame.payload)
            elif frame.packet_id == packet_id(mapping, "login", "clientbound", "login_disconnect"):
                reason, offset = get_string(frame.payload)
                require_end(frame.payload, offset, "login disconnect")
                raise HarnessError(f"server login disconnect: {reason}")
            else:
                raise HarnessError(f"unexpected login packet id {frame.packet_id}")

        write_packet(
            stream,
            packet_id(mapping, "login", "serverbound", "login_acknowledged"),
            b"",
            compression,
        )
        write_packet(stream, serverbound_config["client_information"], make_client_information(), compression)

        config_packets = 0
        play_packets = 0
        keepalives = 0
        protocol_disconnect: str | None = None
        known_pack_reply = False
        config_finished = False
        known_config_clientbound = set(mapping["configuration"]["clientbound"].values())
        config_wire = ConfigurationCriticalTracker(clientbound, known_config_clientbound)
        while True:
            frame = read_frame(stream, compression)
            config_packets += 1
            action = config_wire.observe(frame)
            if action == "disconnect":
                protocol_disconnect = str(config_wire.last_details["reason"])
                break
            if action == "select_known_packs":
                # We intentionally send no guessed pack identifiers. Pumpkin accepts an empty list.
                payload = encode_varint(0)
                write_packet(stream, serverbound_config["select_known_packs"], payload, compression)
                config_wire.record_known_packs_reply(payload)
                known_pack_reply = True
            elif action == "finish_configuration":
                payload = b""
                write_packet(stream, serverbound_config["finish_configuration"], payload, compression)
                config_wire.record_finish_ack(payload)
                config_wire.require_complete()
                config_finished = True
                break
            elif action == "keep_alive":
                keepalive = parse_i64(frame.payload, "configuration keep-alive")
                write_packet(stream, serverbound_config["keep_alive"], struct.pack(">q", keepalive), compression)
            elif action == "ping":
                ping = parse_i32(frame.payload, "configuration ping")
                write_packet(stream, serverbound_config["pong"], struct.pack(">i", ping), compression)

        if protocol_disconnect is not None:
            return {
                "mode": "login",
                "protocol": PROTOCOL,
                "login_success": login_success,
                "login_packets": login_packets,
                "configuration_disconnect": protocol_disconnect,
                "result": "server_disconnect_before_play",
            }
        if not config_finished:
            raise HarnessError("configuration ended without finish_configuration")

        # The acknowledgement transitions the server to Play. Read at least one
        # actual 26.2 play frame; do not call this a real-client success.
        play_ids = {name: packet_id(mapping, "play", "clientbound", name) for name in (
            "disconnect", "keep_alive", "ping"
        )}
        play_keepalive_id = packet_id(mapping, "play", "serverbound", "keep_alive")
        play_pong_id = packet_id(mapping, "play", "serverbound", "pong")
        known_play_clientbound = set(mapping["play"]["clientbound"].values())
        while True:
            frame = read_frame(stream, compression)
            play_packets += 1
            if frame.packet_id not in known_play_clientbound:
                raise HarnessError(f"unknown 26.2 play clientbound packet id {frame.packet_id}")
            if frame.packet_id == play_ids["disconnect"]:
                reason, offset = get_string(frame.payload)
                require_end(frame.payload, offset, "play disconnect")
                protocol_disconnect = reason
                break
            if frame.packet_id == play_ids["keep_alive"]:
                keepalive = parse_i64(frame.payload, "play keep-alive")
                write_packet(stream, play_keepalive_id, struct.pack(">q", keepalive), compression)
                keepalives += 1
                if wait_for_keepalive:
                    break
            elif frame.packet_id == play_ids["ping"]:
                ping = parse_i32(frame.payload, "play ping")
                write_packet(stream, play_pong_id, struct.pack(">i", ping), compression)
            if not wait_for_keepalive:
                break

        result = "protocol_disconnect" if protocol_disconnect is not None else "play_frame_observed"
        return {
            "mode": "login",
            "protocol": PROTOCOL,
            "login_success": login_success,
            "compression_threshold": compression,
            "login_packets": login_packets,
            "configuration_packets": config_packets,
            "known_pack_reply": known_pack_reply,
            "play_packets": play_packets,
            "keepalives_replied": keepalives,
            "result": result,
            "real_client_compatibility": "not_claimed",
        }


def parse_disconnect_reason(payload: bytes, context: str) -> str:
    reason, offset = get_string(payload)
    require_end(payload, offset, context)
    return reason


@dataclass
class ConfigurationCriticalTracker:
    """Admission observer for the small, critical Configuration wire subset.

    Known packet IDs outside this subset are surfaced as ``needs_review``; an
    ID absent from the official 26.2 mapping is a hard failure.  Gameplay
    payloads are deliberately outside this class and are never called decoded.
    """

    ids: dict[str, int]
    all_ids: set[int]
    events: list[dict[str, Any]] | None = None
    # 26.2 omits Update Enabled Features in Pumpkin's configuration path;
    # when present on another mapped version it must precede Known Packs, but
    # it is not a required protocol-776 packet.
    phase: str = "awaiting_known_packs"
    enabled_features_seen: int = 0
    known_packs_seen: int = 0
    known_packs_replies: int = 0
    registry_ids: set[str] | None = None
    update_tags_seen: int = 0
    finish_seen: int = 0
    needs_review_count: int = 0
    last_details: dict[str, Any] | None = None

    def __post_init__(self) -> None:
        if self.events is None:
            self.events = []
        if self.registry_ids is None:
            self.registry_ids = set()

    def _critical_order_error(self, message: str) -> HarnessError:
        return HarnessError(f"Configuration state mismatch: {message}")

    def observe(self, frame: Frame) -> str:
        assert self.registry_ids is not None
        self.last_details = {
            "validation": "decoded",
            "payload_bytes": len(frame.payload),
            "framed_length": frame.framed_length,
            "compressed": frame.compressed,
        }
        if frame.packet_id not in self.all_ids:
            raise HarnessError(
                f"unknown 26.2 configuration clientbound packet id {frame.packet_id}"
            )
        if frame.packet_id == self.ids["disconnect"]:
            reason = parse_disconnect_reason(frame.payload, "configuration disconnect")
            self.last_details.update({"event": "disconnect", "reason": reason})
            return "disconnect"
        if frame.packet_id == self.ids["update_enabled_features"]:
            if self.enabled_features_seen or self.phase != "awaiting_known_packs":
                raise self._critical_order_error("duplicate or late Update Enabled Features")
            self.last_details.update(parse_update_enabled_features(frame.payload))
            self.last_details["event"] = "update_enabled_features"
            self.enabled_features_seen = 1
            return "update_enabled_features"
        if frame.packet_id == self.ids["select_known_packs"]:
            if self.known_packs_seen or self.phase != "awaiting_known_packs":
                raise self._critical_order_error("duplicate or late Known Packs request")
            packs = parse_known_packs(frame.payload)
            self.known_packs_seen = 1
            self.phase = "awaiting_known_packs_reply"
            self.last_details.update({"event": "select_known_packs", "packs": packs})
            return "select_known_packs"
        if frame.packet_id == self.ids["registry_data"]:
            if self.known_packs_replies != 1:
                raise self._critical_order_error("Registry Data before Known Packs reply")
            details = parse_registry_data(frame.payload)
            registry_id = str(details["registry_id"])
            if registry_id in self.registry_ids:
                raise self._critical_order_error(f"duplicate Registry Data: {registry_id}")
            self.registry_ids.add(registry_id)
            self.phase = "registry_data"
            self.last_details.update({"event": "registry_data", **details})
            return "registry_data"
        if frame.packet_id == self.ids["update_tags"]:
            if self.known_packs_replies != 1 or not self.registry_ids:
                raise self._critical_order_error("Update Tags before Registry Data")
            if self.update_tags_seen:
                raise self._critical_order_error("duplicate Update Tags")
            details = parse_update_tags(frame.payload)
            self.update_tags_seen = 1
            self.phase = "update_tags"
            self.last_details.update({"event": "update_tags", **details})
            return "update_tags"
        if frame.packet_id == self.ids["finish_configuration"]:
            if self.finish_seen:
                raise self._critical_order_error("duplicate Finish Configuration")
            if self.known_packs_replies != 1 or not self.registry_ids or not self.update_tags_seen:
                raise self._critical_order_error(
                    "Finish Configuration before Known Packs, Registry Data, and Update Tags"
                )
            require_end(frame.payload, 0, "Finish Configuration")
            self.finish_seen = 1
            self.phase = "awaiting_finish_ack"
            self.last_details["event"] = "finish_configuration"
            return "finish_configuration"
        if frame.packet_id == self.ids["keep_alive"]:
            parse_i64(frame.payload, "configuration keep-alive")
            self.last_details["event"] = "keep_alive"
            return "keep_alive"
        if frame.packet_id == self.ids["ping"]:
            parse_i32(frame.payload, "configuration ping")
            self.last_details["event"] = "ping"
            return "ping"
        self.needs_review_count += 1
        self.last_details.update({
            "validation": "needs_review",
            "event": "needs_review_configuration",
            "packet_id": frame.packet_id,
        })
        return "needs_review_configuration"

    def record_known_packs_reply(self, payload: bytes) -> None:
        if self.known_packs_replies:
            raise self._critical_order_error("duplicate Known Packs reply")
        parse_known_packs(payload)
        if self.phase != "awaiting_known_packs_reply":
            raise self._critical_order_error("Known Packs reply without a request")
        self.known_packs_replies = 1
        self.phase = "awaiting_registry_data"

    def record_finish_ack(self, payload: bytes) -> None:
        require_end(payload, 0, "Finish Configuration acknowledgment")
        if self.finish_seen != 1 or self.phase != "awaiting_finish_ack":
            raise self._critical_order_error("Finish acknowledgment without Finish Configuration")
        self.phase = "finished"

    def require_complete(self) -> None:
        if self.known_packs_seen != 1 or self.known_packs_replies != 1:
            raise HarnessError("critical Configuration packet missing: Known Packs")
        if not self.registry_ids:
            raise HarnessError("critical Configuration packet missing: Registry Data")
        if self.update_tags_seen != 1:
            raise HarnessError("critical Configuration packet missing: Update Tags")
        if self.finish_seen != 1 or self.phase != "finished":
            raise HarnessError("critical Configuration packet missing or unacknowledged: Finish")


@dataclass
class ConfigurationRegressionTracker:
    """Stateful observer for deterministic configuration-order probes.

    This legacy negative-probe observer records packet id/payload bounds and
    disconnect reason. Unsupported but mapped packets are marked
    ``needs_review``; it is not a semantic payload validator.
    """

    ids: dict[str, int]
    events: list[dict[str, Any]] | None = None
    server_phase: str = "awaiting_known_packs"
    known_packs_seen: int = 0
    finish_seen: int = 0
    disconnect_reason: str | None = None

    def __post_init__(self) -> None:
        if self.events is None:
            self.events = []

    def observe(self, frame: Frame) -> str:
        assert self.events is not None
        event: dict[str, Any] = {
            "direction": "clientbound",
            "packet_id": frame.packet_id,
            "payload_bytes": len(frame.payload),
            "framed_length": frame.framed_length,
            "compressed": frame.compressed,
        }
        if frame.packet_id == self.ids["disconnect"]:
            reason = parse_disconnect_reason(frame.payload, "configuration disconnect")
            self.disconnect_reason = reason
            event.update({"event": "disconnect", "reason": reason})
            self.events.append(event)
            return "disconnect"
        if frame.packet_id == self.ids["select_known_packs"]:
            event["packs"] = parse_known_packs(frame.payload)
            self.known_packs_seen += 1
            event["event"] = "select_known_packs"
        elif self.ids.get("update_enabled_features") == frame.packet_id:
            event.update(parse_update_enabled_features(frame.payload))
            event["event"] = "update_enabled_features"
        elif self.ids.get("registry_data") == frame.packet_id:
            event.update(parse_registry_data(frame.payload))
            event["event"] = "registry_data"
        elif self.ids.get("update_tags") == frame.packet_id:
            event.update(parse_update_tags(frame.payload))
            event["event"] = "update_tags"
        elif frame.packet_id == self.ids["finish_configuration"]:
            require_end(frame.payload, 0, "Finish Configuration")
            self.finish_seen += 1
            self.server_phase = "awaiting_finish_ack"
            event["event"] = "finish_configuration"
        elif frame.packet_id == self.ids["keep_alive"]:
            parse_i64(frame.payload, "configuration keep-alive")
            event["event"] = "keep_alive"
        elif frame.packet_id == self.ids["ping"]:
            parse_i32(frame.payload, "configuration ping")
            event["event"] = "ping"
        else:
            event["event"] = "needs_review_configuration"
            event["validation"] = "needs_review"
        self.events.append(event)
        return str(event["event"])

    def record_send(self, event: str, payload: bytes) -> None:
        assert self.events is not None
        self.events.append(
            {
                "direction": "serverbound",
                "event": event,
                "payload_bytes": len(payload),
                "body_is_empty": not payload,
            }
        )

    def require_expected_disconnect(self) -> None:
        if self.disconnect_reason is None:
            raise HarnessError("configuration regression ended without server disconnect")


def _configuration_ids(
    mapping: dict[str, dict[str, dict[str, int]]],
) -> tuple[dict[str, int], dict[str, int], set[int]]:
    clientbound = {
        name: packet_id(mapping, "configuration", "clientbound", name)
        for name in (
            "disconnect",
            "keep_alive",
            "ping",
            "select_known_packs",
            "finish_configuration",
            "update_enabled_features",
            "registry_data",
            "update_tags",
        )
    }
    serverbound = {
        name: packet_id(mapping, "configuration", "serverbound", name)
        for name in ("client_information", "select_known_packs", "finish_configuration", "keep_alive", "pong")
    }
    return clientbound, serverbound, set(mapping["configuration"]["clientbound"].values())


def _login_to_configuration(
    stream: socket.socket,
    timeout: float,
    username: str,
    mapping: dict[str, dict[str, dict[str, int]]],
) -> tuple[int | None, dict[str, Any], dict[str, int], dict[str, int], set[int]]:
    """Perform only the common login prefix; caller owns configuration order."""
    if not 1 <= len(username) <= 16 or not username.isascii() or not username.replace("_", "").isalnum():
        raise HarnessError("username must be 1..16 ASCII alphanumeric/underscore characters")
    clientbound, serverbound, known_config_clientbound = _configuration_ids(mapping)
    write_packet(stream, packet_id(mapping, "handshake", "serverbound", "intention"), make_handshake(2), None)
    write_packet(
        stream,
        packet_id(mapping, "login", "serverbound", "hello"),
        put_string(username, 16 * 4) + uuid.uuid4().bytes,
        None,
    )
    compression: int | None = None
    login_success: dict[str, Any] | None = None
    while login_success is None:
        frame = read_frame(stream, compression)
        if frame.packet_id == packet_id(mapping, "login", "clientbound", "login_compression"):
            threshold, offset = decode_varint(frame.payload)
            require_end(frame.payload, offset, "login compression")
            if threshold < 0 or threshold > MAX_PACKET_DATA_SIZE:
                raise HarnessError(f"invalid compression threshold: {threshold}")
            compression = threshold
        elif frame.packet_id == packet_id(mapping, "login", "clientbound", "login_finished"):
            login_success = parse_login_success(frame.payload)
        elif frame.packet_id == packet_id(mapping, "login", "clientbound", "login_disconnect"):
            raise HarnessError(f"server login disconnect: {parse_disconnect_reason(frame.payload, 'login disconnect')}")
        else:
            raise HarnessError(f"unexpected login packet id {frame.packet_id}")
    write_packet(stream, packet_id(mapping, "login", "serverbound", "login_acknowledged"), b"", compression)
    write_packet(stream, serverbound["client_information"], make_client_information(), compression)
    return compression, login_success, clientbound, serverbound, known_config_clientbound


def _read_expected_config_disconnect(
    stream: socket.socket,
    tracker: ConfigurationRegressionTracker,
    known_config_clientbound: set[int],
    clientbound: dict[str, int],
    serverbound: dict[str, int],
    compression: int | None,
    timeout: float,
) -> None:
    """Read until the server's protocol disconnect, never accept EOF as pass."""
    del timeout  # socket timeout is already installed by the caller
    for _ in range(128):
        frame = read_frame(stream, compression)
        if frame.packet_id not in known_config_clientbound:
            raise HarnessError(f"unknown 26.2 configuration clientbound packet id {frame.packet_id}")
        action = tracker.observe(frame)
        if action == "disconnect":
            tracker.require_expected_disconnect()
            return
        if action == "keep_alive":
            value = parse_i64(frame.payload, "configuration keep-alive")
            payload = struct.pack(">q", value)
            write_packet(stream, serverbound["keep_alive"], payload, compression)
            tracker.record_send("keep_alive", payload)
        elif action == "ping":
            value = parse_i32(frame.payload, "configuration ping")
            payload = struct.pack(">i", value)
            write_packet(stream, serverbound["pong"], payload, compression)
            tracker.record_send("pong", payload)
    raise HarnessError("configuration regression exceeded 128 packets without disconnect")


def run_configuration_regression(
    host: str,
    port: int,
    timeout: float,
    username: str,
    case: str,
    mapping: dict[str, dict[str, dict[str, int]]],
) -> dict[str, Any]:
    """Run an ordered, independent-connection configuration negative probe."""
    if case not in ("early_finish_ack", "duplicate_known_packs"):
        raise HarnessError(f"unsupported configuration regression case: {case}")
    with socket.create_connection((host, port), timeout=timeout) as stream:
        stream.settimeout(timeout)
        compression, login_success, clientbound, serverbound, known_config_clientbound = _login_to_configuration(
            stream, timeout, username, mapping
        )
        tracker = ConfigurationRegressionTracker(clientbound)
        if case == "early_finish_ack":
            # Wait for the server's Known Packs request, then deliberately send
            # Finish Configuration before the server's Finish Configuration.
            for _ in range(128):
                frame = read_frame(stream, compression)
                if frame.packet_id not in known_config_clientbound:
                    raise HarnessError(f"unknown 26.2 configuration clientbound packet id {frame.packet_id}")
                action = tracker.observe(frame)
                if action == "disconnect":
                    raise HarnessError("server disconnected before early Finish Configuration probe")
                if action == "select_known_packs":
                    if tracker.finish_seen:
                        raise HarnessError("early Finish Configuration probe started after server finish")
                    write_packet(stream, serverbound["finish_configuration"], b"", compression)
                    tracker.record_send("early_finish_configuration", b"")
                    break
                if action == "keep_alive":
                    value = parse_i64(frame.payload, "configuration keep-alive")
                    payload = struct.pack(">q", value)
                    write_packet(stream, serverbound["keep_alive"], payload, compression)
                    tracker.record_send("keep_alive", payload)
                elif action == "ping":
                    value = parse_i32(frame.payload, "configuration ping")
                    payload = struct.pack(">i", value)
                    write_packet(stream, serverbound["pong"], payload, compression)
                    tracker.record_send("pong", payload)
            else:
                raise HarnessError("server did not request known packs before early probe")
        else:
            # Complete one valid Known Packs exchange first.  Only after a
            # server response is observed is the duplicate sent, avoiding a
            # timing race with the initial request.
            sent_valid = False
            for _ in range(128):
                frame = read_frame(stream, compression)
                if frame.packet_id not in known_config_clientbound:
                    raise HarnessError(f"unknown 26.2 configuration clientbound packet id {frame.packet_id}")
                action = tracker.observe(frame)
                if action == "disconnect":
                    raise HarnessError("server disconnected before duplicate Known Packs probe")
                if action == "select_known_packs":
                    payload = encode_varint(0)
                    write_packet(stream, serverbound["select_known_packs"], payload, compression)
                    tracker.record_send("known_packs", payload)
                    sent_valid = True
                    break
                if action == "keep_alive":
                    value = parse_i64(frame.payload, "configuration keep-alive")
                    payload = struct.pack(">q", value)
                    write_packet(stream, serverbound["keep_alive"], payload, compression)
                    tracker.record_send("keep_alive", payload)
                elif action == "ping":
                    value = parse_i32(frame.payload, "configuration ping")
                    payload = struct.pack(">i", value)
                    write_packet(stream, serverbound["pong"], payload, compression)
                    tracker.record_send("pong", payload)
            if not sent_valid:
                raise HarnessError("server did not request known packs before duplicate probe")
            response = read_frame(stream, compression)
            if response.packet_id not in known_config_clientbound:
                raise HarnessError(f"unknown 26.2 configuration clientbound packet id {response.packet_id}")
            if tracker.observe(response) == "disconnect":
                raise HarnessError("server disconnected after valid Known Packs before duplicate probe")
            payload = encode_varint(0)
            write_packet(stream, serverbound["select_known_packs"], payload, compression)
            tracker.record_send("duplicate_known_packs", payload)
        _read_expected_config_disconnect(
            stream, tracker, known_config_clientbound, clientbound, serverbound, compression, timeout
        )
        return {
            "mode": "config-regression",
            "case": case,
            "protocol": PROTOCOL,
            "login_success": login_success,
            "compression_threshold": compression,
            "events": tracker.events,
            "server_disconnect": True,
            "disconnect_reason": tracker.disconnect_reason,
            "result": "expected_configuration_disconnect",
            "real_client_compatibility": "not_claimed",
        }


def run_strict_play(
    host: str,
    port: int,
    timeout: float,
    username: str,
    mapping: dict[str, dict[str, dict[str, int]]],
    known_packs_case: str = "empty",
) -> dict[str, Any]:
    """Run first-playable validation with one explicit Known Packs response fixture."""
    if known_packs_case not in ("empty", "legal_subset", "unknown", "exact"):
        raise HarnessError(f"unsupported Known Packs response case: {known_packs_case}")
    if not 1 <= len(username) <= 16 or not username.isascii() or not username.replace("_", "").isalnum():
        raise HarnessError("username must be 1..16 ASCII alphanumeric/underscore characters")
    clientbound = {
        name: packet_id(mapping, "configuration", "clientbound", name)
        for name in (
            "disconnect", "keep_alive", "ping", "select_known_packs", "finish_configuration",
            "update_enabled_features", "registry_data", "update_tags",
        )
    }
    serverbound_config = {
        name: packet_id(mapping, "configuration", "serverbound", name)
        for name in ("client_information", "select_known_packs", "finish_configuration", "keep_alive", "pong")
    }
    with socket.create_connection((host, port), timeout=timeout) as stream:
        stream.settimeout(timeout)
        write_packet(stream, packet_id(mapping, "handshake", "serverbound", "intention"), make_handshake(2), None)
        login_start = put_string(username, 16 * 4) + uuid.uuid4().bytes
        write_packet(stream, packet_id(mapping, "login", "serverbound", "hello"), login_start, None)

        compression: int | None = None
        login_success: dict[str, Any] | None = None
        login_packets = 0
        while login_success is None:
            frame = read_frame(stream, compression)
            login_packets += 1
            if frame.packet_id == packet_id(mapping, "login", "clientbound", "login_compression"):
                threshold, offset = decode_varint(frame.payload)
                require_end(frame.payload, offset, "login compression")
                if threshold < 0 or threshold > MAX_PACKET_DATA_SIZE:
                    raise HarnessError(f"invalid compression threshold: {threshold}")
                compression = threshold
            elif frame.packet_id == packet_id(mapping, "login", "clientbound", "login_finished"):
                login_success = parse_login_success(frame.payload)
            elif frame.packet_id == packet_id(mapping, "login", "clientbound", "login_disconnect"):
                reason, offset = get_string(frame.payload)
                require_end(frame.payload, offset, "login disconnect")
                raise HarnessError(f"server login disconnect: {reason}")
            else:
                raise HarnessError(f"unexpected login packet id {frame.packet_id}")

        write_packet(
            stream,
            packet_id(mapping, "login", "serverbound", "login_acknowledged"),
            b"",
            compression,
        )
        write_packet(stream, serverbound_config["client_information"], make_client_information(), compression)

        config_packets = 0
        known_pack_reply = False
        known_packs_request: list[dict[str, str]] | None = None
        known_packs_response: list[dict[str, str]] | None = None
        config_finished = False
        known_config_clientbound = set(mapping["configuration"]["clientbound"].values())
        config_wire = ConfigurationCriticalTracker(clientbound, known_config_clientbound)
        while True:
            frame = read_frame(stream, compression)
            config_packets += 1
            action = config_wire.observe(frame)
            if action == "disconnect":
                reason = str(config_wire.last_details["reason"])
                raise HarnessError(f"server configuration disconnect: {reason}")
            if action == "select_known_packs":
                known_packs_request = config_wire.last_details["packs"]
                known_packs_response = make_known_packs_response(known_packs_request, known_packs_case)
                payload = encode_known_packs(known_packs_response)
                write_packet(stream, serverbound_config["select_known_packs"], payload, compression)
                config_wire.record_known_packs_reply(payload)
                known_pack_reply = True
            elif action == "finish_configuration":
                payload = b""
                write_packet(stream, serverbound_config["finish_configuration"], payload, compression)
                config_wire.record_finish_ack(payload)
                config_wire.require_complete()
                config_finished = True
                break
            elif action == "keep_alive":
                keepalive = parse_i64(frame.payload, "configuration keep-alive")
                write_packet(stream, serverbound_config["keep_alive"], struct.pack(">q", keepalive), compression)
            elif action == "ping":
                ping = parse_i32(frame.payload, "configuration ping")
                write_packet(stream, serverbound_config["pong"], struct.pack(">i", ping), compression)
        if not config_finished:
            raise HarnessError("configuration ended without finish_configuration")

        play_ids = {
            name: packet_id(mapping, "play", "clientbound", name)
            for name in ("disconnect", "keep_alive", "ping", "login", "level_chunk_with_light", "player_position")
        }
        tracker = StrictPlayTracker(play_ids, set(mapping["play"]["clientbound"].values()))
        play_packets = 0
        play_keepalive_id = packet_id(mapping, "play", "serverbound", "keep_alive")
        play_pong_id = packet_id(mapping, "play", "serverbound", "pong")
        teleport_confirm_id = packet_id(mapping, "play", "serverbound", "accept_teleportation")
        while True:
            frame = read_frame(stream, compression)
            play_packets += 1
            action, value = tracker.accept(frame)
            if action == "teleport_confirm":
                assert value is not None
                write_packet(stream, teleport_confirm_id, encode_varint(value), compression)
                tracker.record_teleport_confirmation(value)
            elif action == "keep_alive":
                assert value is not None
                write_packet(stream, play_keepalive_id, struct.pack(">q", value), compression)
                tracker.record_keepalive_reply()
            elif action == "ping":
                assert value is not None
                write_packet(stream, play_pong_id, struct.pack(">i", value), compression)
            if tracker.join_game is not None and tracker.position_syncs and tracker.first_chunk is not None:
                if tracker.keepalives_replied:
                    tracker.require_complete()
                    break

        tracker.require_complete()
        return {
            "mode": "login",
            "protocol": PROTOCOL,
            "login_success": login_success,
            "compression_threshold": compression,
            "login_packets": login_packets,
            "configuration_packets": config_packets,
            "known_pack_reply": known_pack_reply,
            "known_packs_case": known_packs_case,
            "known_packs_request": known_packs_request,
            "known_packs_response": known_packs_response,
            "known_packs_expected_selection": "exact" if known_packs_case == "exact" else "fallback",
            "play_packets": play_packets,
            "join_game": tracker.join_game,
            "position_syncs": tracker.position_syncs,
            "teleport_confirms_sent": tracker.teleport_confirms,
            "first_chunk": tracker.first_chunk,
            "keepalives_replied": tracker.keepalives_replied,
            "opaque_play_packets": tracker.opaque_packets,
            "strict_play_validation": "pass",
            "result": "first_playable_strict",
            "real_client_compatibility": "not_claimed",
        }


def _positions_equal(left: dict[str, Any] | None, right: dict[str, Any] | None) -> bool:
    if left is None or right is None:
        return left is right
    return all(
        math.isclose(float(left[name]), float(right[name]), rel_tol=0.0, abs_tol=1e-9)
        for name in ("x", "y", "z", "delta_x", "delta_y", "delta_z", "yaw", "pitch")
    ) and left["relatives"] == right["relatives"]


def run_reconfiguration(
    host: str,
    port: int,
    timeout: float,
    username: str,
    mapping: dict[str, dict[str, dict[str, int]]],
    cycles: int = 2,
    known_packs_case: str = "empty",
    case: str = "normal",
) -> dict[str, Any]:
    """Wait for a server-owned Play reconfiguration trigger and validate its wire loop.

    This is opt-in and intentionally never triggers the server. A Rust integration test must
    call ``JavaClient::start_reconfiguration`` for each requested cycle after the baseline
    Play state is established. The harness only proves a synthetic peer exchanged bounded,
    state-correct packets; it does not prove vanilla-client acceptance.
    """
    cases = {
        "normal", "duplicate_ack", "unsolicited_ack", "trailing_ack",
        "early_finish", "duplicate_finish", "disconnect_during_config",
    }
    if case not in cases:
        raise HarnessError(f"unsupported reconfiguration case: {case}")
    if not 1 <= cycles <= 2:
        raise HarnessError("reconfiguration cycles must be 1 or 2")
    if known_packs_case not in ("empty", "legal_subset", "unknown", "exact"):
        raise HarnessError(f"unsupported Known Packs response case: {known_packs_case}")
    if not 1 <= len(username) <= 16 or not username.isascii() or not username.replace("_", "").isalnum():
        raise HarnessError("username must be 1..16 ASCII alphanumeric/underscore characters")

    clientbound, serverbound, known_config_clientbound = _configuration_ids(mapping)
    play_ids = {
        name: packet_id(mapping, "play", "clientbound", name)
        for name in ("disconnect", "keep_alive", "ping", "login", "level_chunk_with_light", "player_position", "start_configuration")
    }
    play_server = {
        name: packet_id(mapping, "play", "serverbound", name)
        for name in ("configuration_acknowledged", "keep_alive", "pong", "accept_teleportation")
    }
    known_play_clientbound = set(mapping["play"]["clientbound"].values())
    events: list[dict[str, Any]] = []

    def record(direction: str, state: str, frame: Frame, event: str) -> None:
        events.append({
            "direction": direction,
            "state": state,
            "event": event,
            "packet_id": frame.packet_id,
            "payload_bytes": len(frame.payload),
            "framed_length": frame.framed_length,
            "compressed": frame.compressed,
        })

    def record_send(state: str, event: str, packet_id_value: int, payload: bytes) -> None:
        events.append({
            "direction": "serverbound",
            "state": state,
            "event": event,
            "packet_id": packet_id_value,
            "payload_bytes": len(payload),
            "body_is_empty": not payload,
        })

    with socket.create_connection((host, port), timeout=timeout) as stream:
        stream.settimeout(timeout)
        compression, login_success, _, config_serverbound, _ = _login_to_configuration(
            stream, timeout, username, mapping
        )

        # Initial Login -> Configuration -> Play has its own codecs and IDs. Do not reuse
        # reconfiguration packet IDs as evidence for this first transition.
        initial_config = ConfigurationCriticalTracker(clientbound, known_config_clientbound)
        while True:
            frame = read_frame(stream, compression)
            action = initial_config.observe(frame)
            if action == "disconnect":
                reason = str(initial_config.last_details["reason"])
                raise HarnessError(f"server initial configuration disconnect: {reason}")
            if action == "select_known_packs":
                request = initial_config.last_details["packs"]
                response = make_known_packs_response(request, known_packs_case)
                payload = encode_known_packs(response)
                write_packet(stream, config_serverbound["select_known_packs"], payload, compression)
                initial_config.record_known_packs_reply(payload)
            elif action == "finish_configuration":
                payload = b""
                write_packet(stream, config_serverbound["finish_configuration"], payload, compression)
                initial_config.record_finish_ack(payload)
                initial_config.require_complete()
                break
            elif action == "keep_alive":
                value = parse_i64(frame.payload, "initial configuration keep-alive")
                payload = struct.pack(">q", value)
                write_packet(stream, config_serverbound["keep_alive"], payload, compression)
            elif action == "ping":
                value = parse_i32(frame.payload, "initial configuration ping")
                payload = struct.pack(">i", value)
                write_packet(stream, config_serverbound["pong"], payload, compression)

        tracker = StrictPlayTracker(
            {name: value for name, value in play_ids.items() if name != "start_configuration"},
            known_play_clientbound,
        )
        play_packets = 0

        def consume_play(frame: Frame) -> str:
            nonlocal play_packets
            play_packets += 1
            if frame.packet_id == play_ids["start_configuration"]:
                require_end(frame.payload, 0, "start configuration")
                record("clientbound", "play", frame, "start_configuration")
                return "start_configuration"
            before_position = tracker.position
            action, value = tracker.accept(frame)
            record("clientbound", "play", frame, action)
            if action == "teleport_confirm":
                assert value is not None
                payload = encode_varint(value)
                write_packet(stream, play_server["accept_teleportation"], payload, compression)
                tracker.record_teleport_confirmation(value)
                if before_position is not None and not _positions_equal(before_position, tracker.position):
                    raise HarnessError("player position changed during reconfiguration")
            elif action == "keep_alive":
                assert value is not None
                payload = struct.pack(">q", value)
                write_packet(stream, play_server["keep_alive"], payload, compression)
                tracker.record_keepalive_reply()
            elif action == "ping":
                assert value is not None
                payload = struct.pack(">i", value)
                write_packet(stream, play_server["pong"], payload, compression)
            return action

        # Establish a strict initial Play baseline before waiting for the server-owned seam.
        while True:
            frame = read_frame(stream, compression)
            action = consume_play(frame)
            if action == "start_configuration":
                raise HarnessError("reconfiguration trigger arrived before initial Play baseline")
            if tracker.join_game is not None and tracker.position_syncs and tracker.first_chunk is not None:
                if tracker.keepalives_replied:
                    tracker.require_complete()
                    break
        baseline_entity_id = tracker.join_game["entity_id"] if tracker.join_game else None
        baseline_position = tracker.position
        _write_diagnostic(
            "play_baseline_ready",
            entity_id=baseline_entity_id,
            position=baseline_position,
            keepalives_replied=tracker.keepalives_replied,
            opaque_play_packets=tracker.opaque_packets,
        )

        def wait_for_play_disconnect() -> str:
            for _ in range(512):
                frame = read_frame(stream, compression)
                if frame.packet_id not in known_play_clientbound:
                    raise HarnessError(f"unknown play packet during negative probe: {frame.packet_id}")
                if frame.packet_id == play_ids["disconnect"]:
                    reason = parse_disconnect_reason(frame.payload, "play disconnect")
                    record("clientbound", "play", frame, "disconnect")
                    return reason
                action = consume_play(frame)
                if action == "start_configuration":
                    raise HarnessError("server started configuration during unsolicited/trailing negative probe")
            raise HarnessError("negative Play probe exceeded 512 packets without disconnect")

        def wait_for_config_disconnect() -> str:
            for _ in range(512):
                frame = read_frame(stream, compression)
                if frame.packet_id not in known_config_clientbound:
                    raise HarnessError(f"unknown configuration packet during negative probe: {frame.packet_id}")
                if frame.packet_id == clientbound["disconnect"]:
                    reason = parse_disconnect_reason(frame.payload, "configuration disconnect")
                    record("clientbound", "configuration", frame, "disconnect")
                    return reason
                if frame.packet_id == clientbound["keep_alive"]:
                    value = parse_i64(frame.payload, "configuration keep-alive")
                    payload = struct.pack(">q", value)
                    write_packet(stream, config_serverbound["keep_alive"], payload, compression)
                    record("clientbound", "configuration", frame, "keep_alive")
                    record_send("configuration", "keep_alive", config_serverbound["keep_alive"], payload)
                elif frame.packet_id == clientbound["ping"]:
                    value = parse_i32(frame.payload, "configuration ping")
                    payload = struct.pack(">i", value)
                    write_packet(stream, config_serverbound["pong"], payload, compression)
                    record("clientbound", "configuration", frame, "ping")
                    record_send("configuration", "pong", config_serverbound["pong"], payload)
                elif frame.packet_id == clientbound["update_enabled_features"]:
                    details = parse_update_enabled_features(frame.payload)
                    record("clientbound", "configuration", frame, "update_enabled_features")
                    events[-1].update(details)
                elif frame.packet_id == clientbound["registry_data"]:
                    details = parse_registry_data(frame.payload)
                    record("clientbound", "configuration", frame, "registry_data")
                    events[-1].update(details)
                elif frame.packet_id == clientbound["update_tags"]:
                    details = parse_update_tags(frame.payload)
                    record("clientbound", "configuration", frame, "update_tags")
                    events[-1].update(details)
                elif frame.packet_id == clientbound["select_known_packs"]:
                    details = parse_known_packs(frame.payload)
                    record("clientbound", "configuration", frame, "select_known_packs")
                    events[-1]["packs"] = details
                elif frame.packet_id == clientbound["finish_configuration"]:
                    require_end(frame.payload, 0, "Finish Configuration")
                    record("clientbound", "configuration", frame, "finish_configuration")
                else:
                    record("clientbound", "configuration", frame, "needs_review_configuration")
                    events[-1]["validation"] = "needs_review"
            raise HarnessError("negative Configuration probe exceeded 512 packets without disconnect")

        def wait_for_start_configuration() -> None:
            for _ in range(2048):
                frame = read_frame(stream, compression)
                action = consume_play(frame)
                if action == "start_configuration":
                    _write_diagnostic("start_configuration_observed", play_packets=play_packets)
                    return
            raise HarnessError("server-owned reconfiguration trigger was not observed within 2048 Play packets")

        if case == "unsolicited_ack":
            payload = b""
            write_packet(stream, play_server["configuration_acknowledged"], payload, compression)
            record_send("play", "unsolicited_configuration_acknowledged", play_server["configuration_acknowledged"], payload)
            reason = wait_for_play_disconnect()
            return {
                "mode": "reconfiguration", "case": case, "protocol": PROTOCOL,
                "login_success": login_success, "baseline_entity_id": baseline_entity_id,
                "baseline_position": baseline_position, "server_disconnect": True,
                "disconnect_reason": reason, "result": "expected_unsolicited_ack_disconnect",
                "real_client_compatibility": "not_claimed",
            }

        completed = 0
        post_reconfiguration_keepalives = 0
        for cycle in range(cycles):
            wait_for_start_configuration()
            payload = b""
            write_packet(stream, play_server["configuration_acknowledged"], payload, compression)
            record_send("play", "configuration_acknowledged", play_server["configuration_acknowledged"], payload)

            if case == "duplicate_ack":
                write_packet(stream, play_server["configuration_acknowledged"], payload, compression)
                record_send("configuration", "duplicate_configuration_acknowledged", play_server["configuration_acknowledged"], payload)
                reason = wait_for_config_disconnect()
                return {
                    "mode": "reconfiguration", "case": case, "protocol": PROTOCOL,
                    "login_success": login_success, "baseline_entity_id": baseline_entity_id,
                    "baseline_position": baseline_position, "server_disconnect": True,
                    "disconnect_reason": reason, "result": "expected_duplicate_ack_disconnect",
                    "real_client_compatibility": "not_claimed",
                }
            if case == "trailing_ack":
                payload = b"\x00"
                write_packet(stream, play_server["configuration_acknowledged"], payload, compression)
                record_send("play", "trailing_configuration_acknowledged", play_server["configuration_acknowledged"], payload)
                reason = wait_for_play_disconnect()
                return {
                    "mode": "reconfiguration", "case": case, "protocol": PROTOCOL,
                    "login_success": login_success, "baseline_entity_id": baseline_entity_id,
                    "baseline_position": baseline_position, "server_disconnect": True,
                    "disconnect_reason": reason, "result": "expected_trailing_ack_disconnect",
                    "real_client_compatibility": "not_claimed",
                }
            if case == "disconnect_during_config":
                stream.shutdown(socket.SHUT_RDWR)
                return {
                    "mode": "reconfiguration", "case": case, "protocol": PROTOCOL,
                    "login_success": login_success, "baseline_entity_id": baseline_entity_id,
                    "baseline_position": baseline_position, "peer_shutdown": "client_eof",
                    "result": "client_disconnected_during_configuration",
                    "real_client_compatibility": "not_claimed",
                }

            config_wire = ConfigurationCriticalTracker(clientbound, known_config_clientbound)
            while True:
                frame = read_frame(stream, compression)
                action = config_wire.observe(frame)
                details = dict(config_wire.last_details or {})
                record("clientbound", "configuration", frame, action)
                events[-1].update(details)
                if action == "disconnect":
                    reason = str(details["reason"])
                    raise HarnessError(f"server reconfiguration disconnect: {reason}")
                if action == "select_known_packs":
                    request = details["packs"]
                    response = make_known_packs_response(request, known_packs_case)
                    payload = encode_known_packs(response)
                    write_packet(stream, config_serverbound["select_known_packs"], payload, compression)
                    config_wire.record_known_packs_reply(payload)
                    record_send("configuration", "select_known_packs", config_serverbound["select_known_packs"], payload)
                    if case == "early_finish":
                        payload = b""
                        write_packet(stream, config_serverbound["finish_configuration"], payload, compression)
                        record_send("configuration", "early_finish_configuration", config_serverbound["finish_configuration"], payload)
                        reason = wait_for_config_disconnect()
                        return {
                            "mode": "reconfiguration", "case": case, "protocol": PROTOCOL,
                            "login_success": login_success, "baseline_entity_id": baseline_entity_id,
                            "baseline_position": baseline_position, "server_disconnect": True,
                            "disconnect_reason": reason, "result": "expected_early_finish_disconnect",
                            "real_client_compatibility": "not_claimed",
                        }
                elif action == "finish_configuration":
                    payload = b""
                    write_packet(stream, config_serverbound["finish_configuration"], payload, compression)
                    config_wire.record_finish_ack(payload)
                    config_wire.require_complete()
                    _write_diagnostic(
                        "configuration_finished",
                        registry_count=len(config_wire.registry_ids or ()),
                        update_tags=config_wire.update_tags_seen,
                        needs_review=config_wire.needs_review_count,
                    )
                    record_send("configuration", "finish_configuration", config_serverbound["finish_configuration"], payload)
                    if case == "duplicate_finish":
                        write_packet(stream, config_serverbound["finish_configuration"], payload, compression)
                        record_send("play", "duplicate_finish_configuration", config_serverbound["finish_configuration"], payload)
                    break
                elif action == "keep_alive":
                    value = parse_i64(frame.payload, "reconfiguration keep-alive")
                    payload = struct.pack(">q", value)
                    write_packet(stream, config_serverbound["keep_alive"], payload, compression)
                    record_send("configuration", "keep_alive", config_serverbound["keep_alive"], payload)
                elif action == "ping":
                    value = parse_i32(frame.payload, "reconfiguration ping")
                    payload = struct.pack(">i", value)
                    write_packet(stream, config_serverbound["pong"], payload, compression)
                    record_send("configuration", "pong", config_serverbound["pong"], payload)

            # The Finish ack must return to the same Play connection. A Join Game is
            # forbidden here; any later position packet must equal the baseline exactly.
            for _ in range(2048):
                frame = read_frame(stream, compression)
                action = consume_play(frame)
                if action == "start_configuration":
                    raise HarnessError("second configuration trigger arrived before Play keep-alive")
                if action == "keep_alive":
                    post_reconfiguration_keepalives += 1
                    completed += 1
                    _write_diagnostic(
                        "returned_play_keepalive",
                        cycle=completed,
                        post_reconfiguration_keepalives=post_reconfiguration_keepalives,
                    )
                    break
            else:
                raise HarnessError("no Play keep-alive observed after reconfiguration Finish ack")

        if tracker.join_game is None or tracker.join_game["entity_id"] != baseline_entity_id:
            raise HarnessError("player/entity identity was not preserved across reconfiguration")
        if not _positions_equal(baseline_position, tracker.position):
            raise HarnessError("player position was not preserved across reconfiguration")
        return {
            "mode": "reconfiguration", "case": case, "protocol": PROTOCOL,
            "login_success": login_success, "compression_threshold": compression,
            "cycles_requested": cycles, "cycles_completed": completed,
            "baseline_entity_id": baseline_entity_id, "baseline_position": baseline_position,
            "same_entity": True, "same_position": True,
            "post_reconfiguration_play_keepalives": post_reconfiguration_keepalives,
            "events": events, "known_packs_case": known_packs_case,
            "critical_wire": {
                "registry_data_packets": sum(
                    event["event"] == "registry_data" for event in events
                ),
                "update_tags_packets": sum(
                    event["event"] == "update_tags" for event in events
                ),
                "needs_review_packets": sum(
                    event.get("validation") == "needs_review" for event in events
                ),
            },
            "result": (
                "duplicate_finish_no_second_transition"
                if case == "duplicate_finish"
                else "stateful_reconfiguration_pass"
            ),
            "real_client_compatibility": "not_claimed",
        }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("status", "login", "config-regression", "reconfiguration"), default="status")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=25565)
    parser.add_argument("--timeout", type=float, default=10.0, help="per socket read/connect timeout")
    parser.add_argument("--username", default="compat_26_2")
    parser.add_argument(
        "--diagnostic-file",
        type=Path,
        help="optional JSON-lines phase trace for integration-test diagnostics",
    )
    parser.add_argument("--wait-for-keepalive", action="store_true")
    parser.add_argument(
        "--case",
        choices=("early_finish_ack", "duplicate_known_packs"),
        help="ordered negative case for --mode config-regression",
    )
    parser.add_argument(
        "--strict-play",
        action="store_true",
        help="opt-in first-playable validation: Join Game, position/teleport confirm, first chunk envelope, keep-alive",
    )
    parser.add_argument(
        "--reconfiguration-case",
        choices=("normal", "duplicate_ack", "unsolicited_ack", "trailing_ack", "early_finish", "duplicate_finish", "disconnect_during_config"),
        default="normal",
        help="opt-in stateful reconfiguration probe; the server-owned Rust seam must trigger it",
    )
    parser.add_argument(
        "--reconfiguration-cycles",
        type=int,
        choices=(1, 2),
        default=2,
        help="number of server-owned Play -> Configuration -> Play cycles to await",
    )
    parser.add_argument(
        "--known-packs-case",
        choices=("empty", "legal_subset", "unknown", "exact"),
        default="empty",
        help="wire-valid Known Packs response fixture used by --strict-play or --mode reconfiguration",
    )
    args = parser.parse_args(argv)
    global _DIAGNOSTIC_FILE, _DIAGNOSTIC_SEQUENCE
    _DIAGNOSTIC_FILE = args.diagnostic_file
    _DIAGNOSTIC_SEQUENCE = 0
    _write_diagnostic("harness_started", mode=args.mode, username=args.username)
    try:
        mapping = _asset_packet_ids()
        verify_critical_mapping(mapping)
        if args.mode == "reconfiguration":
            if args.strict_play or args.wait_for_keepalive:
                raise HarnessError("--mode reconfiguration owns its strict Play baseline; do not combine --strict-play or --wait-for-keepalive")
            result = run_reconfiguration(
                args.host,
                args.port,
                args.timeout,
                args.username,
                mapping,
                args.reconfiguration_cycles,
                args.known_packs_case,
                args.reconfiguration_case,
            )
        elif args.strict_play:
            if args.mode != "login":
                raise HarnessError("--strict-play requires --mode login")
            result = run_strict_play(
                args.host, args.port, args.timeout, args.username, mapping, args.known_packs_case
            )
        elif args.mode == "config-regression":
            if args.case is None:
                raise HarnessError("--mode config-regression requires --case")
            result = run_configuration_regression(
                args.host, args.port, args.timeout, args.username, args.case, mapping
            )
        elif args.mode == "status":
            result = run_status(args.host, args.port, args.timeout, mapping)
        else:
            result = run_login(
                args.host,
                args.port,
                args.timeout,
                args.username,
                args.wait_for_keepalive,
                mapping,
            )
    except (HarnessError, OSError, ValueError) as exc:
        _write_diagnostic("harness_failed", error=str(exc))
        print(f"FAIL: {exc}", file=sys.stderr)
        return 1
    _write_diagnostic("harness_result", result=result.get("result"), cycles_completed=result.get("cycles_completed"))
    print(json.dumps(result, ensure_ascii=False, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
