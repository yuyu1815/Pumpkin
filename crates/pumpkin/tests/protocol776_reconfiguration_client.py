#!/usr/bin/env python3
"""Test-only adapter around the repository's protocol-776 harness.

The server-side test now waits for a child-published Play readiness marker
before calling the Rust trigger. This adapter therefore deliberately does not
union Play packet IDs into the Configuration allowlist: a Play/Configuration
state collision or unknown ID must fail rather than being hidden as opaque.
It never creates or sends a reconfiguration trigger.
"""
from __future__ import annotations

import sys
from pathlib import Path

TOOLS = Path(__file__).resolve().parents[3] / "tools" / "compat-26_2"
sys.path.insert(0, str(TOOLS))

import protocol776_harness as harness  # noqa: E402

raise SystemExit(harness.main(sys.argv[1:]))
