# scripts/

Helper scripts: recording/capture tooling for the screenshots and demo
animated GIF embedded in the README. See also `../screenshots/README.md` for the generated artefacts.

## record.sh

One-command regeneration of every visual artefact: builds the release binary,
captures the eight still PNGs, and renders the demo video.

```sh
scripts/record.sh
```

Requires `cargo`, `uv`, `vhs`, `ttyd` and `ffmpeg` on PATH. Works from any
working directory. After a video run it removes the feeder's leftovers
(`/tmp/ncom-demo-port`, any stray feeder process).

## demo.tape

VHS tape for the feature-tour screencast (`screenshots/demo.webm` +
`screenshots/demo.gif`). This is the source of truth for the tour's scene
list (highlight, search, filter with search-inside-filter, clear). Run
from the repo root:

```sh
vhs scripts/demo.tape
```

## screenshot.tape

VHS tape that captures the eight still images embedded in the README
(`screenshots/{main,hex,search,line,help,hl,filter,help2}.png`) using VHS's
`Screenshot` command, one known state per still:

```sh
vhs scripts/screenshot.tape
```

## serial_feeder.py

Feeds deterministic serial traffic at 5 lines per second into a pty it
creates itself. Used by both tapes: it publishes the pty's device path to a
file so the tape can launch ncom against it.

```sh
uv run scripts/serial_feeder.py --port-file /tmp/ncom-demo-port
ncom --port "$(cat /tmp/ncom-demo-port)"
```

The traffic is seeded/fixed (boot header once, GPS lines ~1 in 5, regular
`[ERROR]` lines) so recordings are reproducible. Exits when the port's read
side goes away (EIO).

It also doubles as the manual-testing fake serial device (see
"Testing with socat" in the main README): launch it with a port file and
point ncom at the published path. It behaves like the removed
fake-serial.py — own pty, deterministic boot log and telemetry — just at a
fixed 5 lines per second rather than randomized batches.

