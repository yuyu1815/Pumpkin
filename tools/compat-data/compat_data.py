#!/usr/bin/env python3
"""Dependency-free validator for the pinned 26.1/26.2 compatibility data.

The 26.1 namespace is comparison-only.  The final target and all acceptance
claims remain 26.2.  Network fetches are opt-in and are always checked against
the commit/URL/hash in manifest.json; remote HEAD is never a golden input.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent
MANIFEST_PATH = ROOT / "manifest.json"
ALLOWED_CLASSIFICATIONS = {
    "exact",
    "allowed_version_delta",
    "known_implementation_gap",
    "unknown",
    "error",
}
STATES = {"handshake", "handshaking", "status", "login", "configuration", "play", "world", "data"}
DIRECTIONS = {"serverbound", "clientbound", "storage", "replay-normalizer"}
RULE_STATUSES = {"verified", "proposed", "blocked"}
FINAL_TARGET = {"minecraft_version": "26.2", "protocol": 776}
SOURCE_261 = {"minecraft_version": "26.1", "protocol": 775}


class CompatDataError(RuntimeError):
    pass


def read_json(path: Path) -> Any:
    try:
        with path.open(encoding="utf-8") as handle:
            return json.load(handle)
    except (OSError, json.JSONDecodeError) as exc:
        raise CompatDataError(f"cannot read JSON {path}: {exc}") from exc


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def normalize_protocol_schema(
    protocol: dict[str, Any],
    *,
    namespace: str,
    source_id: str | None = None,
) -> list[dict[str, Any]]:
    """Project Prismarine's packet mapper into stable state/direction/name/id rows.

    This reads the actual protocol.json mapper; it does not generate a decoder or
    assert that a 26.2 implementation accepts any of these packets.
    """
    rows: list[dict[str, Any]] = []
    state_names = {
        "handshaking": "handshake",
        "status": "status",
        "login": "login",
        "configuration": "configuration",
        "play": "play",
    }
    direction_names = {"toServer": "serverbound", "toClient": "clientbound"}
    for source_state, state_value in protocol.items():
        if source_state == "types":
            continue
        state = state_names.get(source_state)
        if state is None or not isinstance(state_value, dict):
            continue
        for source_direction, direction_value in state_value.items():
            direction = direction_names.get(source_direction)
            if direction is None or not isinstance(direction_value, dict):
                continue
            packet_type = direction_value.get("types", {}).get("packet")
            if not isinstance(packet_type, list) or len(packet_type) != 2:
                raise CompatDataError(f"missing packet container in {source_state}/{source_direction}")
            fields = packet_type[1]
            if not isinstance(fields, list):
                raise CompatDataError(f"invalid packet container in {source_state}/{source_direction}")
            name_field = next((field for field in fields if field.get("name") == "name"), None)
            name_type = name_field.get("type") if isinstance(name_field, dict) else None
            if not isinstance(name_type, list) or len(name_type) != 2 or name_type[0] != "mapper":
                raise CompatDataError(f"missing packet name mapper in {source_state}/{source_direction}")
            mapping = name_type[1].get("mappings")
            if not isinstance(mapping, dict):
                raise CompatDataError(f"invalid packet name mapper in {source_state}/{source_direction}")
            definitions = direction_value.get("types", {})
            for raw_id, name in mapping.items():
                try:
                    packet_id = int(raw_id, 0)
                except (TypeError, ValueError) as exc:
                    raise CompatDataError(f"invalid packet id {raw_id!r}") from exc
                if not isinstance(name, str):
                    raise CompatDataError(f"invalid packet name for id {raw_id!r}")
                row = {
                    "state": state,
                    "direction": direction,
                    "name": name,
                    "packet_id": packet_id,
                    "source_state": source_state,
                }
                schema_type = f"packet_{name}"
                if schema_type in definitions:
                    row["schema_type"] = schema_type
                if source_id:
                    row["source_id"] = source_id
                rows.append(row)
    return sorted(rows, key=lambda row: (row["state"], row["direction"], row["packet_id"]))


def manifest() -> dict[str, Any]:
    value = read_json(MANIFEST_PATH)
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        raise CompatDataError("manifest schema_version must be 1")
    return value


def source_by_id(data: dict[str, Any], source_id: str) -> dict[str, Any]:
    for source in data.get("sources", []):
        if source.get("id") == source_id:
            return source
    raise CompatDataError(f"unknown source id: {source_id}")


def source_path(source: dict[str, Any]) -> Path | None:
    relative = source.get("path") or source.get("cache_path")
    if not isinstance(relative, str) or not relative:
        return None
    if relative.startswith(".cache/"):
        return ROOT / relative
    return ROOT.parent.parent / relative


def allowlist(data: dict[str, Any]) -> dict[str, Any]:
    relative = data.get("allowlist_path")
    if not isinstance(relative, str) or not relative:
        raise CompatDataError("manifest allowlist_path is required")
    value = read_json(ROOT / relative)
    errors = validate_allowlist(value)
    if errors:
        raise CompatDataError("invalid allowlist: " + "; ".join(errors))
    return value


def _has_wildcard(value: Any) -> bool:
    if isinstance(value, str):
        return "*" in value or value.strip() == ""
    if isinstance(value, dict):
        return any(_has_wildcard(item) for item in value.values())
    if isinstance(value, list):
        return any(_has_wildcard(item) for item in value)
    return False


def _valid_packet_id(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and 0 <= value <= 255


def validate_allowlist(value: Any) -> list[str]:
    errors: list[str] = []
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        return ["schema_version must be 1"]
    if value.get("final_target") != {"minecraft_version": "26.2", "protocol": 776, "world_data_version": 4903}:
        errors.append("final_target must remain Java 26.2/protocol 776/world 4903")
    rules = value.get("rules")
    if not isinstance(rules, list) or not rules:
        return errors + ["rules must be a non-empty list"]
    seen: set[str] = set()
    for index, rule in enumerate(rules):
        prefix = f"rules[{index}]"
        if not isinstance(rule, dict):
            errors.append(f"{prefix} must be an object")
            continue
        rule_id = rule.get("rule_id")
        if not isinstance(rule_id, str) or not rule_id or rule_id in seen:
            errors.append(f"{prefix} has missing or duplicate rule_id")
        else:
            seen.add(rule_id)
        status = rule.get("status")
        if status not in RULE_STATUSES:
            errors.append(f"{prefix} has invalid status")
        if rule.get("enabled") is not (status == "verified"):
            errors.append(f"{prefix}.enabled must be true only for verified rules")
        if rule.get("from") != SOURCE_261 or rule.get("to") != FINAL_TARGET:
            errors.append(f"{prefix} must explicitly name 26.1/775 -> 26.2/776")
        if rule.get("state") not in STATES or rule.get("direction") not in DIRECTIONS:
            errors.append(f"{prefix} has invalid state/direction")
        if _has_wildcard(rule):
            errors.append(f"{prefix} contains blank or wildcard matching")
        subject = rule.get("subject")
        if not isinstance(subject, dict) or not isinstance(subject.get("kind"), str) or not isinstance(subject.get("field"), str):
            errors.append(f"{prefix}.subject requires kind and field")
        if not isinstance(rule.get("required_preservation"), list) or not rule["required_preservation"]:
            errors.append(f"{prefix}.required_preservation must be non-empty")
        if status == "verified":
            if not isinstance(subject, dict) or subject.get("kind") != "packet_symbolic_name":
                errors.append(f"{prefix} verified rule must target a packet symbolic name")
            if not isinstance(subject, dict) or not _valid_packet_id(subject.get("packet_id")):
                errors.append(f"{prefix} verified rule requires a concrete packet_id")
            if not isinstance(subject, dict) or not isinstance(subject.get("from_value"), str) or not isinstance(subject.get("to_value"), str):
                errors.append(f"{prefix} verified rule requires exact from_value/to_value")
            if rule.get("comparison_scope") != "comparison_metadata":
                errors.append(f"{prefix} verified rule must be comparison_metadata only")
    return errors


def validate_local(data: dict[str, Any]) -> list[str]:
    errors: list[str] = []
    target = data.get("final_target", {})
    if target != {
        "minecraft_version": "26.2",
        "protocol": 776,
        "world_data_version": 4903,
        "downgrade_server": False,
    }:
        errors.append("final_target must remain Java 26.2/protocol 776/world 4903")
    try:
        errors.extend(validate_allowlist(read_json(ROOT / data["allowlist_path"])))
    except (KeyError, CompatDataError) as exc:
        errors.append(str(exc))

    for source in data.get("sources", []):
        if not isinstance(source.get("license"), str) or not source["license"].strip():
            errors.append(f"source {source.get('id')} lacks license provenance")
        if source.get("kind") == "upstream_json":
            commit = source.get("upstream_commit", "")
            if not isinstance(commit, str) or len(commit) != 40 or any(char not in "0123456789abcdef" for char in commit):
                errors.append(f"source {source.get('id')} lacks a 40-character pinned commit")
        path_value = source.get("path") or source.get("cache_path")
        path = source_path(source)
        if not path_value or path is None:
            continue
        if source.get("distribution") == "ignored_cache_only" and source.get("path"):
            errors.append(f"ignored cache source must not have committed path: {source.get('id')}")
        if not path.is_file():
            errors.append(f"missing local source/cache: {path_value}")
            continue
        actual = sha256_file(path)
        if actual != source.get("sha256"):
            errors.append(
                f"hash mismatch {path_value}: manifest={source.get('sha256')} actual={actual}"
            )
        if source.get("bytes") != path.stat().st_size:
            errors.append(
                f"byte count mismatch {path_value}: manifest={source.get('bytes')} actual={path.stat().st_size}"
            )

    for namespace in ("26.1", "26.2"):
        for relative in data.get("data_files", {}).get(namespace, []):
            path = ROOT / relative
            if not path.is_file():
                errors.append(f"missing {namespace} data file: {relative}")
                continue
            value = read_json(path)
            if value.get("namespace") != namespace:
                errors.append(f"namespace mix-up in {relative}: {value.get('namespace')!r}")
            if value.get("protocol") not in (775, 776):
                errors.append(f"invalid protocol in {relative}")
            if namespace == "26.1" and value.get("protocol") != 775:
                errors.append(f"26.1 data has non-26.1 protocol in {relative}")
            if namespace == "26.2" and value.get("protocol") != 776:
                errors.append(f"26.2 data has non-26.2 protocol in {relative}")
            source_id = value.get("source_id")
            if source_id:
                try:
                    source = source_by_id(data, source_id)
                    if value.get("source_artifact_sha256") != source.get("sha256"):
                        errors.append(f"source hash mismatch in {relative}: source_id={source_id}")
                except CompatDataError as exc:
                    errors.append(str(exc))

    for namespace, relatives in data.get("schema_files", {}).items():
        for relative in relatives:
            path = ROOT / relative
            if not path.is_file():
                errors.append(f"missing {namespace} schema file: {relative}")
                continue
            try:
                value = read_json(path)
                if path.name == "version.json":
                    if value.get("minecraftVersion") != namespace or value.get("version") != 775:
                        errors.append(f"version schema is not pinned 26.1/775: {relative}")
                elif path.name == "protocol.json":
                    rows = normalize_protocol_schema(value, namespace=namespace)
                    if namespace == "26.1" and len(rows) != 257:
                        errors.append(f"26.1 protocol schema entry count expected 257, got {len(rows)}")
                elif path.name in {"items.json", "blocks.json"}:
                    if not isinstance(value, list) or not value:
                        errors.append(f"{relative} must be a non-empty list")
                elif path.name == "recipes.json":
                    if not isinstance(value, dict) or not value:
                        errors.append(f"{relative} must be a non-empty object")
            except CompatDataError as exc:
                errors.append(str(exc))

    local_26_2 = source_by_id(data, "pumpkin-local-packets-26.2")
    packet_path = ROOT.parent.parent / local_26_2["path"]
    try:
        packets = read_json(packet_path)
        count = 0
        for state, directions in packets.items():
            for direction, entries in directions.items():
                ids = sorted(entry["protocol_id"] for entry in entries.values())
                if ids != list(range(len(ids))):
                    errors.append(f"26.2 packet IDs are not dense in {state}/{direction}")
                count += len(entries)
        if count != 256:
            errors.append(f"26.2 packet entry count expected 256, got {count}")
    except CompatDataError as exc:
        errors.append(str(exc))

    scenarios = read_json(ROOT / "fixtures" / "synthetic-scenarios.json")
    if scenarios.get("observed") is not False:
        errors.append("synthetic fixture must declare observed=false")
    for scenario in scenarios.get("scenarios", []):
        if scenario.get("source", {}).get("protocol") != 776:
            errors.append(f"fixture {scenario.get('scenario_id')} is not a 26.2 source")
        if not scenario.get("expected", {}).get("source"):
            errors.append(f"fixture {scenario.get('scenario_id')} lacks expected-value provenance")
        if scenario.get("state") not in STATES or scenario.get("direction") not in DIRECTIONS:
            errors.append(f"fixture {scenario.get('scenario_id')} has invalid state/direction")
    return errors


def _version_pair_is_known(observation: dict[str, Any], data: dict[str, Any]) -> bool:
    final = data["final_target"]
    source = observation.get("source", {})
    target = observation.get("target", {})
    source_pair = (source.get("minecraft_version"), source.get("protocol"))
    return (
        source_pair in {("26.1", 775), ("26.2", 776)}
        and target.get("minecraft_version") == final["minecraft_version"]
        and target.get("protocol") == final["protocol"]
    )


def _decision(classification: str, allow: bool, reason: str, disposition: str) -> dict[str, Any]:
    return {
        "classification": classification,
        "allow": allow,
        "disposition": disposition,
        "reason": reason,
    }


def _metadata_preservation_matches(observation: dict[str, Any], required: set[str]) -> bool:
    """Check supplied before/after metadata, not wire bytes or runtime semantics."""
    before = observation.get("before_metadata")
    after = observation.get("after_metadata")
    if not isinstance(before, dict) or not isinstance(after, dict):
        return False
    if any(key not in before or key not in after for key in required):
        return False
    changed = {key for key in set(before) | set(after) if before.get(key) != after.get(key)}
    # The only intentional metadata change for the verified rule is the label.
    return changed <= {"packet_name"} and all(before[key] == after[key] for key in required)


def evaluate_observation(observation: dict[str, Any], data: dict[str, Any] | None = None) -> dict[str, Any]:
    """Return a fail-closed decision; never trust an input allow=true flag."""
    data = data or manifest()
    required = ("source", "target", "state", "direction", "classification")
    if not isinstance(observation, dict) or any(key not in observation for key in required):
        return _decision("error", False, "missing contract field", "fail")
    if observation["state"] not in STATES or observation["direction"] not in DIRECTIONS:
        return _decision("error", False, "invalid state or direction", "fail")
    classification = observation.get("classification")
    if classification not in ALLOWED_CLASSIFICATIONS:
        return _decision("error", False, "unknown classification", "fail")
    if not _version_pair_is_known(observation, data):
        return _decision("error", False, "missing or mismatched version/protocol", "fail")

    source = observation["source"]
    target = observation["target"]
    if classification == "exact":
        allowed = source == target == FINAL_TARGET
        return _decision(
            classification,
            allowed,
            "same final-target version/protocol" if allowed else "exact requires 26.2 on both sides",
            "pass" if allowed else "fail",
        )
    if classification == "allowed_version_delta":
        try:
            rules = allowlist(data)["rules"]
        except CompatDataError as exc:
            return _decision("error", False, str(exc), "fail")
        rule_id = observation.get("rule_id")
        rule = next((item for item in rules if item.get("rule_id") == rule_id), None)
        if rule is None:
            return _decision(classification, False, "unknown rule_id", "fail")
        if rule["status"] == "proposed":
            return _decision(classification, False, "proposed rule requires review; it is not an allow", "needs_review")
        if rule["status"] == "blocked":
            return _decision(classification, False, "blocked rule cannot be allowed", "fail")
        subject = rule["subject"]
        expected_preservation = set(rule["required_preservation"])
        actual_preservation = observation.get("preserved")
        exact = (
            rule.get("enabled") is True
            and source == rule["from"]
            and target == rule["to"]
            and observation["state"] == rule["state"]
            and observation["direction"] == rule["direction"]
            and observation.get("context") == rule["context"]
            and observation.get("comparison_scope") == rule["comparison_scope"]
            and observation.get("field") == subject["field"]
            and observation.get("packet_id") == subject["packet_id"]
            and _valid_packet_id(observation.get("packet_id"))
            and observation.get("packet_name_from") == subject["from_value"]
            and observation.get("packet_name_to") == subject["to_value"]
            and observation.get("payload") == "unchanged"
            and observation.get("packet_order") == "unchanged"
            and observation.get("field_count") == "unchanged"
            and observation.get("quantity") == "unchanged"
            and observation.get("packet_presence") == "unchanged"
            and isinstance(actual_preservation, list)
            and set(actual_preservation) == expected_preservation
            and len(actual_preservation) == len(expected_preservation)
            and _metadata_preservation_matches(observation, expected_preservation)
        )
        return _decision(
            classification,
            exact,
            "pinned packet-label metadata delta" if exact else "exact rule preconditions/invariants not satisfied",
            "pass" if exact else "fail",
        )
    return _decision(classification, False, "fail-closed category", "fail")


def list_schema(data: dict[str, Any], namespace: str, state: str | None, direction: str | None) -> int:
    source_id = f"prismarine-minecraft-data-{namespace}-protocol"
    source = source_by_id(data, source_id)
    path = source_path(source)
    if path is None:
        raise CompatDataError(f"source {source_id} has no local/cache schema path")
    protocol = read_json(path)
    rows = normalize_protocol_schema(protocol, namespace=namespace, source_id=source_id)
    if state:
        rows = [row for row in rows if row["state"] == state]
    if direction:
        rows = [row for row in rows if row["direction"] == direction]
    print(json.dumps({
        "namespace": namespace,
        "physical_source_version": source.get("physical_source_version"),
        "protocol": 775,
        "source_id": source_id,
        "source_artifact_sha256": source.get("sha256"),
        "count": len(rows),
        "entries": rows,
    }, ensure_ascii=False, sort_keys=True))
    return 0


def fetch_sources(data: dict[str, Any], selected: str | None = None) -> int:
    cache = ROOT / ".cache"
    cache.mkdir(exist_ok=True)
    sources = data.get("sources", [])
    if selected:
        sources = [source_by_id(data, selected)]
    for source in sources:
        url = source.get("url")
        if not url:
            continue
        request = urllib.request.Request(url, headers={"User-Agent": "Pumpkin-compat-data/1"})
        with urllib.request.urlopen(request, timeout=30) as response:
            payload = response.read()
        actual = hashlib.sha256(payload).hexdigest()
        if actual != source.get("sha256"):
            raise CompatDataError(
                f"pinned fetch hash mismatch {source['id']}: expected {source.get('sha256')} got {actual}"
            )
        destination = ROOT / source.get("cache_path", f".cache/{source['id']}.json")
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(payload)
        print(f"fetched {source['id']}: {len(payload)} bytes sha256={actual} -> {destination}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("verify", help="verify local namespaces, hashes, and synthetic contracts")
    fetch = sub.add_parser("fetch", help="fetch only pinned URLs into the ignored .cache directory")
    fetch.add_argument("--source", help="fetch one manifest source id")
    schema = sub.add_parser("schema-list", aliases=["list-schema"], help="list normalized entries from committed protocol.json")
    schema.add_argument("--namespace", default="26.1", choices=["26.1"])
    schema.add_argument("--state", choices=sorted(STATES - {"handshaking", "world", "data"}))
    schema.add_argument("--direction", choices=["serverbound", "clientbound"])
    classify = sub.add_parser("classify", help="classify one observation JSON read from stdin")
    args = parser.parse_args(argv)
    try:
        data = manifest()
        if args.command == "verify":
            errors = validate_local(data)
            if errors:
                for error in errors:
                    print(f"ERROR: {error}", file=sys.stderr)
                return 1
            print("compat-data verify: OK (26.1 comparison namespace, 26.2 final namespace)")
            return 0
        if args.command == "fetch":
            return fetch_sources(data, args.source)
        if args.command in {"schema-list", "list-schema"}:
            return list_schema(data, args.namespace, args.state, args.direction)
        observation = json.load(sys.stdin)
        result = evaluate_observation(observation, data)
        print(json.dumps(result, ensure_ascii=False, sort_keys=True))
        return 0 if result["allow"] else 1
    except (CompatDataError, OSError, json.JSONDecodeError, urllib.error.URLError) as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
