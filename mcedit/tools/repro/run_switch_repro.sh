#!/usr/bin/env bash
# End-to-end, re-runnable evidence pipeline for the camera-switch wrong-frame
# investigation (multi-camera interview with B-frames, different start points,
# a short camera and non-uniform VFR timestamps).
#
# Stages:
#   1. build the Rust core + CLI (the frame-grid fix lives in mcedit-core)
#   2. regenerate the B-frame / VFR / offset / short-camera repro media
#   3. build the interview-switch project (on- and off-grid cuts)
#   4. render it with the fixed binary
#   5. decode every rendered frame's barcode and verify the actually presented
#      source frame against the plan, writing independent failure evidence
#   6. derive the pre-fix (buggy half-open query) boundary predictions and
#      diff them against the post-fix rendered frames ("修补前后对照")
#
# All generated media/renders go under --data (default: a throwaway dir outside
# the repo). The checked-in golden evidence under tools/repro/evidence is only
# refreshed with --save-golden. Re-running with no args must end exit 0.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

DATA_DIR="${MCEDIT_REPRO_DATA:-$ROOT/target/repro-data}"
SAVE_GOLDEN=0
while [ $# -gt 0 ]; do
  case "$1" in
    --data) DATA_DIR="$2"; shift 2 ;;
    --save-golden) SAVE_GOLDEN=1; shift ;;
    -h|--help)
      grep '^#' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

# ffmpeg/ffprobe/cargo may be installed under a local prefix in this environment.
export PATH="${HOME}/.cargo/bin:${HOME}/.local/bin:${PATH}"
if [ -f "${HOME}/local/bin/ffenv.sh" ]; then
  # shellcheck disable=SC1091
  source "${HOME}/local/bin/ffenv.sh"
fi
if [ -n "${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER:-}" ]; then
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER
else
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=/usr/bin/gcc
fi

MCEDIT="$ROOT/target/debug/mcedit"
PROJECT="interview-switch-repro"
GOLDEN_DIR="$ROOT/tools/repro/evidence"

echo "== [1/6] build Rust core + CLI =="
cargo build --quiet

echo "== [2/6] regenerate B-frame / VFR / offset / short-camera media =="
rm -rf "$DATA_DIR"
mkdir -p "$DATA_DIR"
python3 -I tools/repro/make_repro_media.py --out "$DATA_DIR"

echo "== [3/6] build interview-switch project =="
python3 -I tools/repro/build_switch_project.py --data "$DATA_DIR"

echo "== [4/6] render project =="
"$MCEDIT" render "$DATA_DIR/projects/$PROJECT.json" --base "$DATA_DIR" --out "$DATA_DIR/renders"

REVISION="$("$MCEDIT" validate "$DATA_DIR/projects/$PROJECT.json" \
  | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["revision"])')"
MASTER="$DATA_DIR/renders/$PROJECT.$REVISION.master.mkv"
PLAN="$DATA_DIR/renders/$PROJECT.$REVISION.plan.json"
EVIDENCE_DIR="$DATA_DIR/evidence"

echo "== [5/6] verify every rendered frame against the plan =="
python3 -I tools/repro/verify_switch_frames.py \
  --data "$DATA_DIR" --master "$MASTER" --plan "$PLAN" --evidence-dir "$EVIDENCE_DIR"

echo "== [6/6] pre-fix vs post-fix boundary contrast =="
python3 -I tools/repro/boundary_contrast.py \
  --data "$DATA_DIR" --evidence-dir "$EVIDENCE_DIR"

if [ "$SAVE_GOLDEN" -eq 1 ]; then
  mkdir -p "$GOLDEN_DIR"
  cp "$EVIDENCE_DIR/switch-verify-report.json" "$GOLDEN_DIR/"
  cp "$EVIDENCE_DIR/boundary-contrast.json" "$GOLDEN_DIR/"
  cp "$DATA_DIR/decode-evidence.json" "$GOLDEN_DIR/"
  echo "golden evidence refreshed in $GOLDEN_DIR"
fi

echo
echo "SWITCH REPRO PIPELINE PASSED (data: $DATA_DIR)"
