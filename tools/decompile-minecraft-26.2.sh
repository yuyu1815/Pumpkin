#!/usr/bin/env bash
# Download and decompile Mojang's 26.2 server outside this repository.
set -euo pipefail

VERSION=26.2
OUTER_SHA1=823e2250d24b3ddac457a60c92a6a941943fcd6a
INNER_SHA256=183c0499c5f855570ee487dd38e141a53f0121f83a0b07a3bac2d8b6698823e8
INNER_ENTRY=META-INF/versions/26.2/server-26.2.jar
MANIFEST_URL=https://piston-meta.mojang.com/mc/game/version_manifest_v2.json
SERVER_URL=https://piston-data.mojang.com/v1/objects/${OUTER_SHA1}/server.jar
CFR_VERSION=0.152
CFR_URL=https://www.benf.org/other/cfr/cfr-${CFR_VERSION}.jar

usage() {
    cat <<'EOF'
Usage: tools/decompile-minecraft-26.2.sh OUTPUT_DIRECTORY [--full]

Downloads Mojang's official 26.2 server bundle, verifies it, extracts its
nested implementation, and decompiles a focused Warden source set.  --full
decompiles the complete nested server JAR instead. OUTPUT_DIRECTORY must be
outside the Pumpkin repository.

Set CFR_JAR=/path/to/cfr-0.152.jar to reuse a cached CFR JAR.
EOF
}

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

sha1_matches() {
    [[ "$(sha1sum "$1" | awk '{print tolower($1)}')" == "$2" ]]
}

sha256_matches() {
    [[ "$(sha256sum "$1" | awk '{print tolower($1)}')" == "$2" ]]
}

download() {
    local url=$1 destination=$2 temporary="${2}.tmp"
    curl --fail --location --retry 3 --connect-timeout 30 --max-time 900 \
        --output "$temporary" "$url"
    mv "$temporary" "$destination"
}

require_jdk21() {
    local java_major javac_major
    java_major=$(java -version 2>&1 | awk -F'[".]' '/version/ { print $2; exit }')
    javac_major=$(javac -version 2>&1 | awk -F'[ .]' '{ print $2; exit }')
    [[ "$java_major" == 21 && "$javac_major" == 21 ]] \
        || fail "JDK 21 is required; found java=${java_major:-unknown}, javac=${javac_major:-unknown}"
}

[[ $# -ge 1 ]] || { usage >&2; exit 2; }
[[ $# -le 2 ]] || { usage >&2; exit 2; }
OUT_DIR=$1
MODE=focused
if [[ ${2:-} == --full ]]; then
    MODE=full
elif [[ $# -eq 2 ]]; then
    usage >&2
    exit 2
fi

for command in curl unzip sha1sum sha256sum java javac awk grep find tr realpath; do
    command -v "$command" >/dev/null || fail "required command not found: $command"
done
require_jdk21

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd -P)
canonical_out_dir=$(realpath -m -- "$OUT_DIR") \
    || fail "could not canonicalize OUTPUT_DIRECTORY safely"
[[ -n "$canonical_out_dir" ]] \
    || fail "could not canonicalize OUTPUT_DIRECTORY safely"
OUT_DIR=$canonical_out_dir
[[ "$OUT_DIR" != "$REPO_ROOT" && "$OUT_DIR" != "$REPO_ROOT"/* ]] \
    || fail "OUTPUT_DIRECTORY must be outside the Pumpkin repository"
if [[ -e "$OUT_DIR" || -L "$OUT_DIR" ]]; then
    [[ -d "$OUT_DIR" ]] \
        || fail "OUTPUT_DIRECTORY exists and is not a directory; choose a new path"
    [[ -z "$(find "$OUT_DIR" -mindepth 1 -print -quit)" ]] \
        || fail "OUTPUT_DIRECTORY is not empty; choose a new or cleared path"
fi
mkdir -p -- "$OUT_DIR"

CACHE_DIR="$OUT_DIR/cache"
WORK_DIR="$OUT_DIR/work"
SOURCE_DIR="$OUT_DIR/src"
mkdir -p -- "$CACHE_DIR" "$WORK_DIR"

MANIFEST="$CACHE_DIR/version_manifest_v2.json"
METADATA="$CACHE_DIR/${VERSION}.json"
BUNDLE="$CACHE_DIR/server-${VERSION}-bundle.jar"
INNER_JAR="$CACHE_DIR/server-${VERSION}-inner.jar"

download "$MANIFEST_URL" "$MANIFEST"
metadata_record=$(tr '{}' '\n' <"$MANIFEST" | awk -F '"' '
    $4 == "26.2" && $8 == "release" {
        for (i = 1; i <= NF; i++) {
            if ($i == "url") url = $(i + 2)
            if ($i == "sha1") sha1 = $(i + 2)
        }
        print url "\t" sha1
    }
')
IFS=$'\t' read -r METADATA_URL METADATA_SHA1 <<<"$metadata_record"
[[ -n "${METADATA_URL:-}" && -n "${METADATA_SHA1:-}" ]] \
    || fail "official version manifest has no release metadata for ${VERSION}"

if [[ ! -f "$METADATA" ]] || ! sha1_matches "$METADATA" "$METADATA_SHA1"; then
    download "$METADATA_URL" "$METADATA"
fi
sha1_matches "$METADATA" "$METADATA_SHA1" \
    || fail "26.2 metadata SHA-1 verification failed against the official manifest"
grep -Fq "$SERVER_URL" "$METADATA" \
    || fail "26.2 metadata does not contain the pinned server artifact URL"
grep -Fq "$OUTER_SHA1" "$METADATA" \
    || fail "26.2 metadata does not contain the pinned server SHA-1"

if [[ ! -f "$BUNDLE" ]] || ! sha1_matches "$BUNDLE" "$OUTER_SHA1"; then
    download "$SERVER_URL" "$BUNDLE"
fi
sha1_matches "$BUNDLE" "$OUTER_SHA1" \
    || fail "outer server bundle SHA-1 verification failed"

if [[ ! -f "$INNER_JAR" ]] || ! sha256_matches "$INNER_JAR" "$INNER_SHA256"; then
    unzip -Z1 "$BUNDLE" | grep -Fx "$INNER_ENTRY" >/dev/null \
        || fail "outer bundle does not contain $INNER_ENTRY"
    unzip -p "$BUNDLE" "$INNER_ENTRY" >"${INNER_JAR}.tmp"
    mv "${INNER_JAR}.tmp" "$INNER_JAR"
fi
sha256_matches "$INNER_JAR" "$INNER_SHA256" \
    || fail "inner server JAR SHA-256 verification failed"

CFR_JAR=${CFR_JAR:-"$CACHE_DIR/cfr-${CFR_VERSION}.jar"}
if [[ ! -f "$CFR_JAR" ]]; then
    [[ "$CFR_JAR" == "$CACHE_DIR"/* ]] \
        || fail "CFR_JAR does not exist: $CFR_JAR"
    download "$CFR_URL" "$CFR_JAR"
fi
CFR_VERSION_OUTPUT=$(java -jar "$CFR_JAR" --version 2>&1)
printf '%s\n' "$CFR_VERSION_OUTPUT" | grep -Fq "${CFR_VERSION}" \
    || fail "CFR JAR is not version ${CFR_VERSION}"

mkdir -p -- "$SOURCE_DIR" "$WORK_DIR/classes"
if [[ "$MODE" == full ]]; then
    java -jar "$CFR_JAR" "$INNER_JAR" --outputdir "$SOURCE_DIR" --silent true
fi

# Decompile the indexed classes separately so their stable paths also exist after --full.
targets=(
    net/minecraft/util/SpawnUtil.class
    'net/minecraft/util/SpawnUtil$Strategy.class'
    net/minecraft/world/entity/Mob.class
    net/minecraft/world/entity/monster/warden/Warden.class
    net/minecraft/world/level/block/SculkShriekerBlock.class
    net/minecraft/world/level/block/entity/SculkShriekerBlockEntity.class
)
warden_tracker=$(unzip -Z1 "$INNER_JAR" | grep -E '/WardenSpawnTracker\.class$' || true)
[[ -n "$warden_tracker" ]] || fail "WardenSpawnTracker.class was not found in the inner server JAR"
targets+=("$warden_tracker")

for entry in "${targets[@]}"; do
    unzip -Z1 "$INNER_JAR" | grep -Fx "$entry" >/dev/null \
        || fail "required class not found: $entry"
    class_file="$WORK_DIR/classes/$entry"
    mkdir -p -- "$(dirname -- "$class_file")"
    unzip -p "$INNER_JAR" "$entry" >"$class_file"
    java -jar "$CFR_JAR" "$class_file" --extraclasspath "$INNER_JAR" \
        --outputdir "$SOURCE_DIR" --silent true
done
find_source() {
    local name=$1 path
    path=$(find "$SOURCE_DIR" -type f -name "$name" -print | head -n 1 || true)
    [[ -n "$path" ]] || fail "CFR did not produce ${name}"
    printf '%s' "${path#"$OUT_DIR"/}"
}

spawn_util=$(find_source SpawnUtil.java)
spawn_strategy=$(find_source 'SpawnUtil$Strategy.java')
mob=$(find_source Mob.java)
warden=$(find_source Warden.java)
warden_tracker_source=$(find_source WardenSpawnTracker.java)
shrieker_block=$(find_source SculkShriekerBlock.java)
shrieker_entity=$(find_source SculkShriekerBlockEntity.java)
JAVA_VERSION_OUTPUT=$(java -version 2>&1 | head -n 2)
CFR_SHA256=$(sha256sum "$CFR_JAR" | awk '{print tolower($1)}')

cat >"$OUT_DIR/README.md" <<EOF
# Minecraft Java ${VERSION} server decompilation

Generated by \
\`$(basename "$SCRIPT_DIR")/decompile-minecraft-${VERSION}.sh\` on $(date -u +%Y-%m-%dT%H:%M:%SZ).

- Mode: \`${MODE}\`
- Official version manifest: <${MANIFEST_URL}>
- Official ${VERSION} metadata: <${METADATA_URL}> (SHA-1 \`${METADATA_SHA1}\`)
- Official server bundle: <${SERVER_URL}>
- Outer \`server.jar\` SHA-1: \`${OUTER_SHA1}\`
- Inner \`${INNER_ENTRY}\` SHA-256: \`${INNER_SHA256}\`
- CFR: \`${CFR_VERSION_OUTPUT}\`
- CFR JAR SHA-256: \`${CFR_SHA256}\`
- Java used:
\`\`\`text
${JAVA_VERSION_OUTPUT}
\`\`\`

The official artifacts and generated source remain in this external directory;
do not add them to the Pumpkin repository. See [INDEX.md](INDEX.md).
EOF

cat >"$OUT_DIR/INDEX.md" <<EOF
# Warden source index (Minecraft Java ${VERSION})

- [SpawnUtil](${spawn_util})
- [SpawnUtil\$Strategy](${spawn_strategy})
- [Mob](${mob})
- [Warden](${warden})
- [WardenSpawnTracker](${warden_tracker_source})
- [SculkShriekerBlock](${shrieker_block})
- [SculkShriekerBlockEntity](${shrieker_entity})

Useful searches from this directory:

\`\`\`sh
rg -n -C 20 'trySpawnMob|ON_TOP_OF_COLLIDER' src/net/minecraft/util/SpawnUtil.java
rg -n -C 12 'checkSpawnRules|checkSpawnObstruction|isFree|noCollision' src/net/minecraft/world/entity/Mob.java
rg -n -C 12 'checkSpawnObstruction|noCollision' src/net/minecraft/world/entity/monster/warden/Warden.java
rg -n -C 16 'trySpawnMob|WardenSpawnTracker' src/net/minecraft/world/level/block/SculkShriekerBlock.java src/net/minecraft/world/entity/monster/warden/WardenSpawnTracker.java
rg -n -C 12 'tryRespond|warningLevel|canSummon' src/net/minecraft/world/level/block/SculkShriekerBlock.java src/net/minecraft/world/level/block/entity/SculkShriekerBlockEntity.java
\`\`\`
EOF

printf 'ready: %s\n' "$OUT_DIR"
printf 'index: %s/INDEX.md\n' "$OUT_DIR"
