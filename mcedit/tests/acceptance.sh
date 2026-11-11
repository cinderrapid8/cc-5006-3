#!/usr/bin/env bash
# Full acceptance pipeline for mcedit.
#
#   1. cargo build + cargo test (anchor mapping, gaps, rapid cuts, damage)
#   2. regenerate the three-camera corpus (visible counters + barcodes + audio)
#   3. build the five project variants
#   4. render + verify each one (frame continuity, sound/picture, reproducibility)
#   5. headless browser-logic tests under Node
#
# Usage: PATH must include ffmpeg/ffprobe and cargo.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="${HOME}/.cargo/bin:${HOME}/.local/bin:${PATH}"

echo "== [1/5] Rust build and tests =="
cargo build --quiet
cargo test --quiet

echo "== [2/5] regenerate test media =="
rm -rf data/media data/projects data/renders data/tmp data/media-manifest.json
python3 -I tools/make_media.py --out data

echo "== [3/5] build project variants =="
python3 -I tools/build_projects.py --data data

echo "== [4/5] render and verify every project =="
for p in news-main news-fallback news-rapid news-damaged news-uncertain; do
  echo "--- $p"
  python3 -I tools/verify_render.py --data data --project "$p" --mcedit ./target/debug/mcedit \
    | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['pass'], d; print('  pass:', {k:d[k] for k in ('total_frames','black_frames','fallback_frames','audio_blips','reproducible')})"
done

echo "== [5/5] browser timeline logic =="
node tests/browser_logic_test.js

echo
echo "ALL ACCEPTANCE CHECKS PASSED"
