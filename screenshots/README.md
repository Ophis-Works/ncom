# screenshots

Visual artefacts embedded in the main README.

| File | What it is |
| --- | --- |
| `main.png` `hex.png` `search.png` `line.png` `help.png` `hl.png` `filter.png` `help2.png` | Stills embedded in the README; regenerate with `vhs scripts/screenshot.tape` |
| `demo.webm` | Master screencast; regenerate with `vhs scripts/demo.tape` (or `scripts/record.sh`) |
| `demo.gif` | Native-resolution GIF embedded in the README; produced by `vhs scripts/demo.tape` |

## Usage

Regenerate everything with one command:

```sh
scripts/record.sh
```

It needs `cargo`, `uv`, `vhs`, `ttyd` and `ffmpeg` on your `PATH` (vhs drives
ttyd; vhs uses ffmpeg internally for encoding).

The tape (`scripts/demo.tape`) is the source of truth for the tour's scene
list.
