import socket
import struct
import unittest
import uuid
import zlib
from unittest.mock import patch

import protocol776_harness as h


class VarIntTests(unittest.TestCase):
    def test_signed_varint_round_trip(self):
        for value in (0, 1, 127, 128, 255, 2_097_152, -1, -(1 << 31), (1 << 31) - 1):
            encoded = h.encode_varint(value)
            decoded, offset = h.decode_varint(encoded)
            self.assertEqual((decoded, offset), (value, len(encoded)))

    def test_varint_rejects_sixth_byte(self):
        with self.assertRaises(h.HarnessError):
            h.decode_varint(b"\x80\x80\x80\x80\x80\x00")


class FramingTests(unittest.TestCase):
    def test_uncompressed_frame_round_trip(self):
        frame = h.frame_packet(0x2A, b"abc", None)
        left, right = socket.socketpair()
        try:
            left.sendall(frame)
            parsed = h.read_frame(right, None)
        finally:
            left.close()
            right.close()
        self.assertEqual((parsed.packet_id, parsed.payload, parsed.compressed), (0x2A, b"abc", False))

    def test_compressed_frame_round_trip(self):
        payload = b"A" * 512
        frame = h.frame_packet(0x2A, payload, 256)
        left, right = socket.socketpair()
        try:
            left.sendall(frame)
            parsed = h.read_frame(right, 256)
        finally:
            left.close()
            right.close()
        self.assertEqual((parsed.packet_id, parsed.payload, parsed.compressed), (0x2A, payload, True))

    def test_compression_threshold_uses_raw_packet_size(self):
        frame = h.frame_packet(0x2A, b"x" * 254, 256)
        # ID (1 byte) + payload (254) is 255, therefore data_length=0/raw.
        body_length, offset = h.decode_varint(frame)
        body = frame[offset : offset + body_length]
        data_length, data_offset = h.decode_varint(body)
        self.assertEqual(data_length, 0)
        self.assertEqual(body[data_offset:], h.encode_varint(0x2A) + b"x" * 254)

    def test_frame_limit_is_enforced_before_reading_body(self):
        left, right = socket.socketpair()
        try:
            left.sendall(h.encode_varint(h.MAX_PACKET_SIZE + 1))
            with self.assertRaises(h.HarnessError):
                h.read_frame(right, None)
        finally:
            left.close()
            right.close()

    def test_compressed_length_mismatch_is_rejected(self):
        raw = h.encode_varint(0x2A) + b"payload"
        body = h.encode_varint(len(raw) + 1) + zlib.compress(raw)
        frame = h.encode_varint(len(body)) + body
        left, right = socket.socketpair()
        try:
            left.sendall(frame)
            with self.assertRaises(h.HarnessError):
                h.read_frame(right, 1)
        finally:
            left.close()
            right.close()


class StructureTests(unittest.TestCase):
    @staticmethod
    def minimal_join_game() -> bytes:
        return (
            struct.pack(">i", 42)
            + b"\x00"
            + h.encode_varint(1)
            + h.put_string("minecraft:overworld")
            + h.encode_varint(20)
            + h.encode_varint(10)
            + h.encode_varint(10)
            + b"\x00\x01\x00"
            + h.encode_varint(0)
            + h.put_string("minecraft:overworld")
            + struct.pack(">q", 123)
            + b"\x00"
            + b"\xff"
            + b"\x00\x00\x00"
            + h.encode_varint(0)
            + h.encode_varint(64)
            + b"\x00\x00"
        )

    @staticmethod
    def minimal_chunk() -> bytes:
        payload = bytearray(struct.pack(">ii", 0, 0) + h.encode_varint(3))
        for key in (1, 4, 5):
            payload.extend(h.encode_varint(key))
            payload.extend(h.encode_varint(37))
            payload.extend(b"\x00" * (37 * 8))
        payload.extend(h.encode_varint(0))  # opaque section data length
        payload.extend(h.encode_varint(0))  # block entities
        payload.extend(h.encode_varint(0) * 4)  # four empty light bitsets
        payload.extend(h.encode_varint(0))  # sky arrays
        payload.extend(h.encode_varint(0))  # block arrays
        return bytes(payload)

    def test_strict_parses_minimal_envelopes(self):
        join = h.parse_join_game(self.minimal_join_game())
        self.assertEqual(join["dimension_name"], "minecraft:overworld")
        self.assertEqual(join["sea_level"], 64)
        position = h.parse_player_position(
            h.encode_varint(7)
            + struct.pack(">ddddddff", 1.0, 64.0, 2.0, 0.0, 0.0, 0.0, 90.0, 0.0)
            + struct.pack(">i", 0)
        )
        self.assertEqual(position["teleport_id"], 7)
        chunk = h.parse_first_chunk(self.minimal_chunk())
        self.assertEqual(chunk["heightmaps"], [1, 4, 5])
        self.assertEqual(chunk["section_data_bytes"], 0)

    def test_strict_skips_block_entity_nbt_without_claiming_semantics(self):
        payload = bytearray(struct.pack(">ii", 0, 0) + h.encode_varint(3))
        for key in (1, 4, 5):
            payload.extend(h.encode_varint(key))
            payload.extend(h.encode_varint(37))
            payload.extend(bytes([0]) * (37 * 8))
        payload.extend(h.encode_varint(0))  # opaque section data length
        payload.extend(h.encode_varint(1))  # one block entity
        payload.extend(bytes([0]))  # packed xz
        payload.extend(struct.pack(">h", 64))
        payload.extend(h.encode_varint(1))  # opaque type id
        payload.extend(bytes([10, 0, 0, 0]))  # empty named compound NBT
        payload.extend(h.encode_varint(0) * 4)
        payload.extend(h.encode_varint(0))
        payload.extend(h.encode_varint(0))
        parsed = h.parse_first_chunk(bytes(payload))
        self.assertEqual(parsed["block_entities"], 1)

    def test_strict_rejects_missing_packet_and_invalid_state(self):
        ids = {"disconnect": 32, "keep_alive": 44, "ping": 61, "login": 49, "level_chunk_with_light": 45, "player_position": 72}
        tracker = h.StrictPlayTracker(ids, set(ids.values()))
        with self.assertRaises(h.HarnessError):
            tracker.accept(h.Frame(72, b"", 1, False))
        tracker.join_game = h.parse_join_game(self.minimal_join_game())
        tracker.position_syncs = 1
        tracker.first_chunk = h.parse_first_chunk(self.minimal_chunk())
        tracker.record_teleport_confirmation(7)
        with self.assertRaisesRegex(h.HarnessError, "keepalive_reply"):
            tracker.require_complete()

    def test_strict_rejects_chunk_size_and_timeout(self):
        bad_chunk = self.minimal_chunk()[:-1] + h.encode_varint(99)
        with self.assertRaises(h.HarnessError):
            h.parse_first_chunk(bad_chunk)

        class TimeoutStream:
            def recv(self, _size):
                raise socket.timeout("test timeout")

        with self.assertRaisesRegex(h.HarnessError, "socket timeout"):
            h.read_exact(TimeoutStream(), 1)

    def test_client_information_has_expected_26_2_shape(self):
        payload = h.make_client_information()
        locale, offset = h.get_string(payload, 0, 64)
        self.assertEqual(locale, "en_us")
        self.assertEqual(payload[offset], 2)
        self.assertEqual(len(payload), offset + 1 + 1 + 1 + 1 + 1 + 1 + 1)

    def test_mapping_is_read_from_existing_26_2_asset(self):
        mapping = h._asset_packet_ids()
        h.verify_critical_mapping(mapping)
        self.assertEqual(h.packet_id(mapping, "status", "clientbound", "status_response"), 0)
        self.assertEqual(h.packet_id(mapping, "play", "clientbound", "keep_alive"), 44)
        self.assertEqual(h.packet_id(mapping, "play", "clientbound", "start_configuration"), 118)
        self.assertEqual(h.packet_id(mapping, "play", "serverbound", "configuration_acknowledged"), 16)

    def test_reconfiguration_position_comparison_is_exactly_bounded(self):
        position = {
            "x": 1.0,
            "y": 64.0,
            "z": -2.0,
            "delta_x": 0.0,
            "delta_y": 0.0,
            "delta_z": 0.0,
            "yaw": 90.0,
            "pitch": 0.0,
            "relatives": 0,
        }
        same = dict(position)
        same["x"] += 1e-10
        changed = dict(position)
        changed["x"] += 1e-6
        self.assertTrue(h._positions_equal(position, same))
        self.assertFalse(h._positions_equal(position, changed))
        self.assertFalse(h._positions_equal(position, None))

    def test_status_payload_requires_protocol_776(self):
        payload = h.put_string('{"version":{"name":"26.2","protocol":776}}')
        self.assertEqual(h.parse_status(payload)["version"]["protocol"], 776)
        bad = h.put_string('{"version":{"name":"26.1","protocol":775}}')
        with self.assertRaises(h.HarnessError):
            h.parse_status(bad)

    def test_known_packs_wire_fixtures_preserve_exact_and_fallback_shapes(self):
        requested = [
            {"namespace": "minecraft", "id": "core", "version": "26.2"},
            {"namespace": "minecraft", "id": "bundle", "version": "26.2"},
        ]
        self.assertEqual(h.parse_known_packs(h.encode_known_packs(requested)), requested)
        subset = h.make_known_packs_response(requested, "legal_subset")
        self.assertEqual(subset, requested[:1])
        self.assertEqual(h.parse_known_packs(h.encode_known_packs(subset)), subset)
        unknown = h.make_known_packs_response(requested, "unknown")
        self.assertEqual(unknown[-1]["namespace"], "example")
        self.assertEqual(h.parse_known_packs(h.encode_known_packs(unknown)), unknown)
        self.assertEqual(h.make_known_packs_response(requested, "empty"), [])
        self.assertEqual(h.make_known_packs_response(requested, "exact"), requested)

    def test_known_packs_wire_parser_keeps_count_and_trailing_bounds(self):
        with self.assertRaisesRegex(h.HarnessError, "outside 0..64"):
            h.parse_known_packs(h.encode_varint(65))
        with self.assertRaisesRegex(h.HarnessError, "trailing bytes"):
            h.parse_known_packs(h.encode_known_packs([]) + b"x")

    def test_resource_pack_ack_fixture_is_local_structure_only(self):
        pack_uuid = uuid.UUID("12345678-1234-5678-1234-567812345678")
        for result in (0, 3, 4, 7, 99):
            fixture = h.synthetic_resource_pack_ack_fixture(pack_uuid, result)
            self.assertEqual(
                h.parse_resource_pack_ack_fixture(fixture),
                {"uuid": str(pack_uuid), "result": result, "download_observed": False},
            )
        with self.assertRaisesRegex(h.HarnessError, "trailing bytes"):
            h.parse_resource_pack_ack_fixture(
                h.synthetic_resource_pack_ack_fixture(pack_uuid, 0) + b"x"
            )

    def test_configuration_regression_tracker_decodes_disconnect_and_order(self):
        ids = {"disconnect": 0, "keep_alive": 4, "ping": 5, "select_known_packs": 14, "finish_configuration": 3}
        tracker = h.ConfigurationRegressionTracker(ids)
        self.assertEqual(tracker.observe(h.Frame(14, b"opaque", 7, False)), "select_known_packs")
        tracker.record_send("known_packs", h.encode_varint(0))
        self.assertEqual(tracker.observe(h.Frame(45, b"registry", 9, True)), "opaque_configuration")
        tracker.record_send("duplicate_known_packs", h.encode_varint(0))
        reason = h.put_string("Received known packs without a pending server request")
        self.assertEqual(tracker.observe(h.Frame(0, reason, len(reason) + 1, False)), "disconnect")
        tracker.require_expected_disconnect()
        self.assertEqual(tracker.disconnect_reason, "Received known packs without a pending server request")
        self.assertEqual([event["direction"] for event in tracker.events], ["clientbound", "serverbound", "clientbound", "serverbound", "clientbound"])

    def test_configuration_disconnect_decoder_rejects_trailing_bytes(self):
        ids = {"disconnect": 0, "keep_alive": 4, "ping": 5, "select_known_packs": 14, "finish_configuration": 3}
        tracker = h.ConfigurationRegressionTracker(ids)
        with self.assertRaisesRegex(h.HarnessError, "trailing bytes"):
            tracker.observe(h.Frame(0, h.put_string("reason") + b"x", 8, False))


if __name__ == "__main__":
    unittest.main()
