#!/usr/bin/env python3
"""Read-only local evidence probe for a possible vanilla 26.2 comparison.

This helper deliberately does not launch Minecraft, contact Mojang/Microsoft,
read launcher accounts/logs/credentials, accept an EULA, or modify any path.
It reports only metadata needed to decide whether a later, user-authorized
comparison is unblocked.  Paths are redacted before they are emitted.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import subprocess
import sys
from pathlib import Path
from typing import Any, Iterable

EXPECTED = {
    "version": "26.2",
    "protocol": 776,
    "world_data_version": 4903,
    "metadata_sha1": "987b91a95ae93b3bb78cc14d6e0bbd31bad08d59",
    "client_sha1": "2dc72797acbc1b63fc16a11c4ac393605f453754",
    "client_bytes": 39193383,
    "server_sha1": "823e2250d24b3ddac457a60c92a6a941943fcd6a",
    "server_bytes": 60894273,
    "java_major": 25,
}


def _path(value: str | os.PathLike[str] | None) -> Path | None:
    return Path(value).expanduser() if value else None


def _redact(path: Path | None) -> str | None:
    if path is None:
        return None
    value = str(path.resolve())
    replacements = []
    for name in ("USERPROFILE", "APPDATA", "LOCALAPPDATA", "TEMP"):
        raw = os.environ.get(name)
        if raw:
            replacements.append((str(Path(raw).resolve()), f"<{name}>"))
    home = Path.home()
    replacements.append((str(home.resolve()), "<USER_HOME>"))
    for raw, label in sorted(replacements, key=lambda item: len(item[0]), reverse=True):
        if value.casefold().startswith(raw.casefold()):
            return label + value[len(raw) :]
    return value


def _sha1(path: Path) -> tuple[int, str]:
    digest = hashlib.sha1()
    size = 0
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
            size += len(chunk)
    return size, digest.hexdigest()


def _file_record(path: Path, expected_bytes: int | None = None, expected_sha1: str | None = None) -> dict[str, Any]:
    record: dict[str, Any] = {"path": _redact(path), "present": path.is_file()}
    if not path.is_file():
        return record
    size, sha1 = _sha1(path)
    record.update({"bytes": size, "sha1": sha1})
    if expected_bytes is not None:
        record["expected_bytes"] = expected_bytes
        record["bytes_match"] = size == expected_bytes
    if expected_sha1 is not None:
        record["expected_sha1"] = expected_sha1
        record["sha1_match"] = sha1 == expected_sha1
    return record


def _standard_paths() -> dict[str, list[Path]]:
    appdata = _path(os.environ.get("APPDATA"))
    localappdata = _path(os.environ.get("LOCALAPPDATA"))
    program_files = _path(os.environ.get("ProgramFiles")) or Path("C:/Program Files")
    program_files_x86 = _path(os.environ.get("ProgramFiles(x86)")) or Path("C:/Program Files (x86)")
    minecraft = (appdata / ".minecraft") if appdata else None
    paths: dict[str, list[Path]] = {
        "minecraft_roots": [p for p in (minecraft, localappdata / ".minecraft" if localappdata else None) if p],
        "launchers": [
            p
            for p in (
                localappdata / "Programs/Minecraft Launcher/MinecraftLauncher.exe" if localappdata else None,
                program_files / "Minecraft Launcher/MinecraftLauncher.exe",
                program_files_x86 / "Minecraft Launcher/MinecraftLauncher.exe",
            )
            if p
        ],
        "java": [
            p
            for p in (
                program_files / "Java/jdk-25/bin/java.exe",
                program_files / "Java/jdk-21/bin/java.exe",
                program_files_x86 / "Minecraft Launcher/runtime/java-runtime-epsilon/windows-x64/java-runtime-epsilon/bin/java.exe",
                program_files_x86 / "Minecraft Launcher/runtime/java-runtime-delta/windows-x64/java-runtime-delta/bin/java.exe",
            )
            if p
        ],
    }
    return paths


def _java_version(executable: Path) -> dict[str, Any]:
    record: dict[str, Any] = {"path": _redact(executable), "present": executable.is_file()}
    if not executable.is_file():
        return record
    try:
        completed = subprocess.run(
            [str(executable), "-version"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        record["probe_error"] = type(exc).__name__
        return record
    text = (completed.stdout + "\n" + completed.stderr).splitlines()
    first = next((line.strip() for line in text if "version" in line.lower()), "")
    match = re.search(r"version\s+[\"']?([0-9]+)", first, re.IGNORECASE)
    major = int(match.group(1)) if match else None
    record.update({"exit_code": completed.returncode, "version_line_present": bool(first), "major": major})
    record["required_major_match"] = major == EXPECTED["java_major"]
    return record


def _metadata_record(version_json: Path) -> dict[str, Any]:
    record: dict[str, Any] = {"path": _redact(version_json), "present": version_json.is_file()}
    if not version_json.is_file():
        return record
    try:
        data = json.loads(version_json.read_text(encoding="utf-8"))
        downloads = data.get("downloads", {})
        client = downloads.get("client", {})
        server = downloads.get("server", {})
        record.update(
            {
                "id": data.get("id"),
                "java_major": data.get("javaVersion", {}).get("majorVersion"),
                "client_sha1": client.get("sha1"),
                "client_bytes": client.get("size"),
                "server_sha1": server.get("sha1"),
                "server_bytes": server.get("size"),
                "metadata_matches_expected": (
                    data.get("id") == EXPECTED["version"]
                    and data.get("javaVersion", {}).get("majorVersion") == EXPECTED["java_major"]
                    and client.get("sha1") == EXPECTED["client_sha1"]
                    and client.get("size") == EXPECTED["client_bytes"]
                    and server.get("sha1") == EXPECTED["server_sha1"]
                    and server.get("size") == EXPECTED["server_bytes"]
                ),
            }
        )
    except (OSError, UnicodeError, json.JSONDecodeError, AttributeError):
        record["parse_error"] = True
    return record


def _first_existing(paths: Iterable[Path]) -> Path | None:
    return next((path for path in paths if path.is_file()), None)


def _source_hash(repo: Path | None) -> str | None:
    if repo is None:
        return None
    try:
        completed = subprocess.run(
            ["git", "-C", str(repo), "rev-parse", "HEAD"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    value = completed.stdout.strip()
    return value if completed.returncode == 0 and re.fullmatch(r"[0-9a-f]{40}", value) else None


def collect_evidence(
    *,
    repo: Path | None = None,
    minecraft_dir: Path | None = None,
    launcher_exe: Path | None = None,
    server_dir: Path | None = None,
    run_java: bool = True,
) -> dict[str, Any]:
    paths = _standard_paths()
    mc = minecraft_dir or _first_existing(paths["minecraft_roots"]) or paths["minecraft_roots"][0]
    version_dir = mc / "versions" / EXPECTED["version"]
    client_jar = version_dir / f"{EXPECTED['version']}.jar"
    version_json = version_dir / f"{EXPECTED['version']}.json"
    launcher_candidates = ([launcher_exe] if launcher_exe else []) + paths["launchers"]
    launcher = _first_existing(launcher_candidates)
    server_candidates = [
        *( [server_dir / "server.jar"] if server_dir else [] ),
        mc / "versions" / EXPECTED["version"] / "server.jar",
        Path("C:/Temp/minecraft-26_2-official/server.jar"),
        Path("C:/Temp/minecraft-26_2/server.jar"),
    ]
    server_jar = _first_existing(server_candidates)
    eula_candidates = [mc / "eula.txt"]
    if server_dir:
        eula_candidates.append(server_dir / "eula.txt")
    eula_candidates.extend(
        [Path("C:/Temp/minecraft-26_2-official/eula.txt"), Path("C:/Temp/minecraft-26_2/eula.txt")]
    )
    eula = _first_existing(eula_candidates)

    java_records = []
    seen: set[str] = set()
    for candidate in paths["java"]:
        key = str(candidate).casefold()
        if key in seen:
            continue
        seen.add(key)
        java_records.append(_java_version(candidate) if run_java else {"path": _redact(candidate), "present": candidate.is_file()})

    client_record = _file_record(client_jar, EXPECTED["client_bytes"], EXPECTED["client_sha1"])
    metadata = _metadata_record(version_json)
    result: dict[str, Any] = {
        "schema": "pumpkin.compat.vanilla-evidence.v1",
        "version": EXPECTED["version"],
        "source_commit": _source_hash(repo),
        "official_metadata": {
            "package_sha1": EXPECTED["metadata_sha1"],
            "server_expected": {"bytes": EXPECTED["server_bytes"], "sha1": EXPECTED["server_sha1"]},
            "client_expected": {"bytes": EXPECTED["client_bytes"], "sha1": EXPECTED["client_sha1"]},
            "java_major": EXPECTED["java_major"],
        },
        "official_client_installation": {
            "minecraft_dir": _redact(mc),
            "launcher_profile_file_present": (mc / "launcher_profiles.json").is_file(),
            "version_metadata": metadata,
            "client_jar": client_record,
        },
        "official_server_artifact": _file_record(server_jar, EXPECTED["server_bytes"], EXPECTED["server_sha1"])
        if server_jar
        else {"present": False, "searched": [_redact(p) for p in server_candidates]},
        "launcher": {"present": launcher is not None, "executable": _redact(launcher)},
        "java": {
            "required_major": EXPECTED["java_major"],
            "candidates": java_records,
            "matching_runtime_present": any(item.get("required_major_match") for item in java_records),
        },
        "eula_file": {"present": eula is not None, "path": _redact(eula)},
        "screen_and_start": {
            "os": platform.platform(aliased=True),
            "interactive_session": bool(os.environ.get("SESSIONNAME") or os.environ.get("DISPLAY")),
            "launcher_start_attempted": False,
            "client_start_attempted": False,
            "screen_rendering": "not tested; launch prohibited until legal/EULA gate is explicit",
        },
        "safety": {
            "network_access": "not used",
            "authentication_or_session_data": "not read or logged",
            "eula_or_login_action": "not performed",
            "existing_server_or_world": "not started or modified",
        },
    }
    blockers = [
        "official client/server usage entitlement and EULA acceptance are not established by filesystem presence",
    ]
    if not client_record.get("present") or not client_record.get("sha1_match") or not client_record.get("bytes_match"):
        blockers.append("official 26.2 client JAR is absent or does not match the expected SHA-1/size")
    if not result["official_server_artifact"].get("present"):
        blockers.append("official 26.2 server.jar is not present in the inspected local locations")
    if not result["eula_file"]["present"]:
        blockers.append("no existing eula.txt was found in the inspected client/server locations")
    if not result["java"]["matching_runtime_present"]:
        blockers.append("Java 25 runtime was not found")
    result["blockers"] = blockers
    result["vanilla_client_compatibility"] = "not tested; do not claim client operation"
    result["offline_synthetic_fallback"] = "prepared: use existing protocol776_harness.py and strict-play only against a separately authorized local server"
    return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--minecraft-dir", type=Path)
    parser.add_argument("--launcher-exe", type=Path)
    parser.add_argument("--server-dir", type=Path)
    parser.add_argument("--no-java-exec", action="store_true", help="inspect java.exe presence without running java -version")
    parser.add_argument("--output", type=Path, help="write JSON evidence to this path")
    args = parser.parse_args(argv)
    evidence = collect_evidence(
        repo=args.repo,
        minecraft_dir=args.minecraft_dir,
        launcher_exe=args.launcher_exe,
        server_dir=args.server_dir,
        run_java=not args.no_java_exec,
    )
    text = json.dumps(evidence, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(text, encoding="utf-8")
    print(text, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
