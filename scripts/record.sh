#!/usr/bin/env bash
# record.sh -- rebuild ncom and regenerate every visual artifact from scratch:
# the 5 README still PNGs (screenshot.py) and the demo screencast (demo.tape
# via VHS: screenshots/demo.mp4 + screenshots/demo.gif).
# Works from any CWD; resolves the repo root from this script's own path.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "== cargo build --release =="
cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"

cd "$SCRIPT_DIR"

echo "== screenshots (screenshot.tape via vhs) =="

(cd "$REPO_ROOT" && vhs scripts/screenshot.tape)
pkill -f '[s]erial_feeder.py' 2>/dev/null || true
rm -f /tmp/ncom-demo-port

echo "== demo screencast (demo.tape via vhs) =="
# The tape starts scripts/serial_feeder.py itself (via uv) and launches
# ./target/release/ncom, so it must run from the repo root.
(cd "$REPO_ROOT" && vhs scripts/demo.tape)
# vhs tears the session down with the whole process group, so the feeder
# cannot clean up after itself: remove its leftovers here.
pkill -f '[s]erial_feeder.py' 2>/dev/null || true
rm -f /tmp/ncom-demo-port

echo
echo "== artifacts =="
ls -lh "$REPO_ROOT"/screenshots/*.png
ls -lh "$REPO_ROOT"/screenshots/demo.webm "$REPO_ROOT"/screenshots/demo.gif
