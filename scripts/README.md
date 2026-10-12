# scripts/

Helper scripts: recording/capture tooling for the screenshots and demo video
embedded in the README, plus a standalone fake serial device for manual
testing. See also `../screenshots/README.md` for the generated artefacts.

## record.sh

One-command regeneration of every visual artefact: builds the release binary,
captures the five still PNGs, and renders the demo video.

```sh
scripts/record.sh
```

Requires `cargo`, `uv`, `vhs`, `ttyd` and `ffmpeg` on PATH. Works from any
working directory. After a video run it removes the feeder's leftovers
(`/tmp/ncom-demo-port`, any stray feeder process).

## demo.tape

VHS tape for the feature-tour screencast (`screenshots/demo.mp4` +
`screenshots/demo.gif`). This is the source of truth for the tour's scene
list (timestamps, highlight, search, filter, hex, local echo, line mode,
help). Run from the repo root:

```sh
vhs scripts/demo.tape
```

## screenshot.tape

VHS tape that captures the five still images embedded in the README
(`screenshots/{main,hex,search,line,help}.png`) using VHS's `Screenshot`
command, one known state per still:

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

