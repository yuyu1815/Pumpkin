import copy
import unittest

import compat_data as c


class LocalDataTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.data = c.manifest()

    def test_local_manifest_namespaces_and_rules(self):
        self.assertEqual(c.validate_local(self.data), [])
        rules = c.allowlist(self.data)["rules"]
        self.assertEqual(len(rules), 7)
        self.assertEqual([rule for rule in rules if rule["status"] == "verified"], [])
        label_rule = next(rule for rule in rules if rule["rule_id"] == "ALW-2601-REPLAY-PACKET-LABEL-62")
        self.assertEqual((label_rule["status"], label_rule["enabled"]), ("proposed", False))

    def test_final_target_is_not_261(self):
        self.assertEqual(self.data["final_target"]["minecraft_version"], "26.2")
        self.assertEqual(self.data["final_target"]["protocol"], 776)
        self.assertFalse(self.data["final_target"]["downgrade_server"])

    def test_261_and_262_have_separate_protocol_summaries(self):
        p261 = c.read_json(c.ROOT / "data/26.1/protocol-summary.json")
        p262 = c.read_json(c.ROOT / "data/26.2/protocol-summary.json")
        self.assertEqual((p261["namespace"], p261["protocol"]), ("26.1", 775))
        self.assertEqual((p262["namespace"], p262["protocol"]), ("26.2", 776))
        self.assertNotEqual(p261["total_packet_entries"], p262["total_packet_entries"])

    def test_committed_261_schema_is_real_and_normalizable(self):
        source = c.source_by_id(self.data, "prismarine-minecraft-data-26.1-protocol")
        protocol = c.read_json(c.source_path(source))
        rows = c.normalize_protocol_schema(protocol, namespace="26.1")
        self.assertEqual(len(rows), 257)
        spectate = [row for row in rows if row["state"] == "play" and row["direction"] == "serverbound" and row["packet_id"] == 62]
        self.assertEqual(spectate[0]["name"], "spectate_entity")
        self.assertEqual(spectate[0]["schema_type"], "packet_spectate_entity")

    def test_synthetic_fixture_is_not_observed(self):
        fixture = c.read_json(c.ROOT / "fixtures/synthetic-scenarios.json")
        self.assertFalse(fixture["observed"])
        self.assertEqual(len(fixture["scenarios"]), 3)
        self.assertTrue(all(item["source"]["protocol"] == 776 for item in fixture["scenarios"]))


class ClassificationTests(unittest.TestCase):
    def exact(self, **changes):
        value = {
            "source": {"minecraft_version": "26.2", "protocol": 776},
            "target": {"minecraft_version": "26.2", "protocol": 776},
            "state": "play",
            "direction": "serverbound",
            "classification": "exact",
        }
        value.update(changes)
        return value

    def label_delta(self, **changes):
        value = {
            "source": {"minecraft_version": "26.1", "protocol": 775},
            "target": {"minecraft_version": "26.2", "protocol": 776},
            "state": "play",
            "direction": "serverbound",
            "classification": "allowed_version_delta",
            "rule_id": "ALW-2601-REPLAY-PACKET-LABEL-62",
            "context": "replay-normalizer",
            "comparison_scope": "comparison_metadata",
            "field": "packet_name",
            "packet_id": 62,
            "packet_name_from": "spectate_entity",
            "packet_name_to": "spectator_action",
            "payload": "unchanged",
            "packet_order": "unchanged",
            "field_count": "unchanged",
            "quantity": "unchanged",
            "packet_presence": "unchanged",
            "preserved": ["packet_id", "payload", "packet_order", "field_count", "quantity", "packet_presence"],
            "before_metadata": {
                "packet_id": 62,
                "packet_name": "spectate_entity",
                "payload": "opaque-payload-hash:fixed-fixture",
                "packet_order": 3,
                "field_count": 1,
                "quantity": 1,
                "packet_presence": "present",
            },
            "after_metadata": {
                "packet_id": 62,
                "packet_name": "spectator_action",
                "payload": "opaque-payload-hash:fixed-fixture",
                "packet_order": 3,
                "field_count": 1,
                "quantity": 1,
                "packet_presence": "present",
            },
        }
        value.update(changes)
        return value

    def test_exact_final_target_allows(self):
        result = c.evaluate_observation(self.exact())
        self.assertEqual(result["allow"], True)
        self.assertEqual(result["disposition"], "pass")

    def test_disabled_label_delta_requires_review(self):
        result = c.evaluate_observation(self.label_delta())
        self.assertFalse(result["allow"])
        self.assertEqual(result["disposition"], "needs_review")

    def test_before_after_metadata_diff_rejects_fake_preservation(self):
        observation = self.label_delta()
        required = set(observation["preserved"])
        self.assertTrue(c._metadata_preservation_matches(observation, required))
        observation["after_metadata"]["quantity"] = 2
        self.assertFalse(c._metadata_preservation_matches(observation, required))

    def test_delta_negative_matrix_fails_closed(self):
        cases = [
            ("wrong state", {"state": "login"}),
            ("wrong direction", {"direction": "clientbound"}),
            ("out of range id", {"packet_id": 256}),
            ("wrong packet name", {"packet_name_to": "other_packet"}),
            ("payload schema scope", {"comparison_scope": "payload_schema"}),
            ("order changed", {"packet_order": "changed"}),
            ("packet removed", {"packet_presence": "removed"}),
            ("packet added", {"packet_presence": "added"}),
            ("item quantity changed", {"quantity": "changed"}),
            ("field count changed", {"field_count": "changed"}),
            ("missing preservation", {"preserved": []}),
            ("changed before-after metadata", {"after_metadata": {"packet_id": 62, "packet_name": "spectator_action", "payload": "changed", "packet_order": 3, "field_count": 1, "quantity": 1, "packet_presence": "present"}}),
            ("missing before-after metadata", {"before_metadata": None}),
            ("unknown rule", {"rule_id": "DOES-NOT-EXIST"}),
        ]
        for label, changes in cases:
            with self.subTest(label=label):
                result = c.evaluate_observation(self.label_delta(**changes))
                self.assertFalse(result["allow"])
                self.assertIn(result["disposition"], {"fail", "needs_review"})

    def test_proposed_and_blocked_rules_do_not_allow(self):
        proposed = self.label_delta(rule_id="ALW-2601-REPLAY-HANDSHAKE-LEGACY-254")
        proposed.update({"state": "handshaking", "packet_id": 254})
        result = c.evaluate_observation(proposed)
        self.assertFalse(result["allow"])
        self.assertEqual(result["disposition"], "needs_review")

        blocked = self.label_delta(rule_id="ALW-2601-DATA-ID-REMAP", state="data", direction="replay-normalizer")
        result = c.evaluate_observation(blocked)
        self.assertFalse(result["allow"])
        self.assertEqual(result["disposition"], "fail")

    def test_unknown_and_known_gap_fail_closed(self):
        for classification in ("unknown", "known_implementation_gap"):
            with self.subTest(classification=classification):
                result = c.evaluate_observation(self.exact(classification=classification))
                self.assertFalse(result["allow"])
                self.assertEqual(result["disposition"], "fail")

    def test_missing_and_mismatched_version_fail_closed(self):
        missing = self.exact()
        del missing["source"]["protocol"]
        result = c.evaluate_observation(missing)
        self.assertEqual(result["classification"], "error")
        self.assertFalse(result["allow"])

        mismatched = self.exact(source={"minecraft_version": "26.1", "protocol": 776})
        result = c.evaluate_observation(mismatched)
        self.assertEqual(result["classification"], "error")
        self.assertFalse(result["allow"])

        wrong_target = self.exact(target={"minecraft_version": "26.1", "protocol": 775})
        result = c.evaluate_observation(wrong_target)
        self.assertEqual(result["classification"], "error")
        self.assertFalse(result["allow"])

    def test_input_allow_flag_is_not_trusted(self):
        result = c.evaluate_observation(self.exact(classification="unknown", allow=True))
        self.assertFalse(result["allow"])

    def test_invalid_state_and_direction_fail_closed(self):
        for changes in ({"state": "not-a-state"}, {"direction": "not-a-direction"}):
            with self.subTest(changes=changes):
                result = c.evaluate_observation(self.exact(**changes))
                self.assertEqual(result["classification"], "error")
                self.assertFalse(result["allow"])


class AllowlistSchemaTests(unittest.TestCase):
    def test_duplicate_rule_id_is_invalid(self):
        rules = c.allowlist(c.manifest())
        broken = copy.deepcopy(rules)
        broken["rules"].append(copy.deepcopy(broken["rules"][0]))
        self.assertTrue(any("duplicate rule_id" in error for error in c.validate_allowlist(broken)))

    def test_wildcard_and_invalid_verified_schema_are_invalid(self):
        rules = c.allowlist(c.manifest())
        broken = copy.deepcopy(rules)
        broken["rules"][0]["subject"]["from_value"] = "*"
        self.assertTrue(any("wildcard" in error for error in c.validate_allowlist(broken)))

        broken = copy.deepcopy(rules)
        broken["rules"][0]["status"] = "verified"
        broken["rules"][0]["enabled"] = True
        broken["rules"][0]["subject"].pop("packet_id")
        self.assertTrue(any("concrete packet_id" in error for error in c.validate_allowlist(broken)))


if __name__ == "__main__":
    unittest.main()
