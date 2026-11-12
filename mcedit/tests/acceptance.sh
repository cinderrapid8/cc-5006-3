#!/usr/bin/env bash
# Full acceptance pipeline for mcedit.
#
#   1. cargo build + cargo test (anchor mapping, gaps, rapid cuts, damage)
#   2. regenerate the three-camera corpus (visible counters + barcodes + audio)
#   3. build the five project variants
#   4. render + verify each one (frame continuity, sound/picture, reproducibility)
#   5. headless browser-logic tests under Node
#   6. camera-switch frame-accuracy repro (B-frame / VFR / offset / short cam)
#
# Usage: PATH must include ffmpeg/ffprobe and cargo.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="${HOME}/.cargo/bin:${HOME}/.local/bin:${PATH}"

# In constrained environments ffmpeg/ffprobe live under a local prefix.
if [ -f "${HOME}/local/bin/ffenv.sh" ]; then
  # shellcheck disable=SC1091
  source "${HOME}/local/bin/ffenv.sh"
fi
# Use the system gcc when the default cc wrapper is unusable.
if [ -z "${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER:-}" ] && [ -x /usr/bin/gcc ]; then
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=/usr/bin/gcc
fi

echo "== [1/6] Rust build and tests =="
cargo build --quiet
cargo test --quiet

echo "== [2/6] regenerate test media =="
rm -rf data/media data/projects data/renders data/tmp data/media-manifest.json
python3 -I tools/make_media.py --out data

echo "== [3/6] build project variants =="
python3 -I tools/build_projects.py --data data

echo "== [4/6] render and verify every project =="
for p in news-main news-fallback news-rapid news-damaged news-uncertain; do
  echo "--- $p"
  python3 -I tools/verify_render.py --data data --project "$p" --mcedit ./target/debug/mcedit \
    | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['pass'], d; print('  pass:', {k:d[k] for k in ('total_frames','black_frames','fallback_frames','audio_blips','reproducible')})"
done

echo "== [5/6] browser timeline logic =="
node tests/browser_logic_test.js

echo "== [6/6] camera-switch frame-accuracy repro =="
# Self-contained B-frame / different-start / VFR / short-camera pipeline with
# independent before/after boundary evidence. Uses a throwaway data dir so no
# untrusted generated media is written into the repo or imported from cwd.
MCEDIT_REPRO_DATA="${ROOT}/target/repro-acceptance" \
  bash tools/repro/run_switch_repro.sh

echo
echo "ALL ACCEPTANCE CHECKS PASSED"
