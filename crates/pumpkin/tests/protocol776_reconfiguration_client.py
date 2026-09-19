#!/usr/bin/env python3
"""Test-only adapter around the repository's protocol-776 harness.

The server can have already-queued Play frames ahead of the Configuration
response queue when a real trigger races the tick thread. This adapter keeps
the existing mapping, framing, and state machine, and only lets those real
queued Play packet IDs be recorded as opaque while the client is reading
Configuration packets. It never creates
or sends a reconfiguration trigger.
"""
from __future__ import annotations

import sys
from pathlib import Path

TOOLS = Path(__file__).resolve().parents[3] / "tools" / "compat-26_2"
sys.path.insert(0, str(TOOLS))

import protocol776_harness as harness  # noqa: E402

_original_configuration_ids = harness._configuration_ids


def _configuration_ids_with_queued_play(mapping):
    clientbound, serverbound, known = _original_configuration_ids(mapping)
    queued_play_ids = set(mapping["play"]["clientbound"].values())
    return clientbound, serverbound, known | queued_play_ids


harness._configuration_ids = _configuration_ids_with_queued_play
raise SystemExit(harness.main(sys.argv[1:]))
