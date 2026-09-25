#!/usr/bin/env python3
"""Focused source-level regression check for the 26.2 /particle packet fix."""
from pathlib import Path
import re

source = Path("crates/pumpkin/src/command/commands/particle.rs").read_text()
match = re.search(
    r"fn supports_empty_particle_options\(particle: Particle\) -> bool \{(.*?)\n\}",
    source,
    re.S,
)
assert match, "command must gate particles without encoded option data"
rejected = set(re.findall(r"Particle::(\w+)", match.group(1)))
expected = {
    "Block", "BlockMarker", "BlockCrumble", "Dust", "DustColorTransition",
    "DustPillar", "DragonBreath", "Effect", "EntityEffect", "FallingDust",
    "Flash", "Geyser", "GeyserBase", "GeyserPlume", "GeyserPoof",
    "InstantEffect", "Item", "SculkCharge", "Shriek", "TintedLeaves",
    "Trail", "Vibration",
}
assert rejected == expected, f"option-bearing particle coverage differs: {rejected ^ expected}"
assert "if !supports_empty_particle_options(particle)" in source
assert re.search(r"CParticle::new\(\s*false,\s*force,", source), (
    "force must encode overrideLimiter first; alwaysShow must remain false"
)
print(f"ok: rejected {len(rejected)} data-carrying particle types; force flags use vanilla order")
