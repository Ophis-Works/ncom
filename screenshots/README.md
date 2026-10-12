# screenshots

Visual artefacts embedded in the main README, plus the palette used for
custom GIF encoding.

| File | What it is |
| --- | --- |
| `main.png` `hex.png` `search.png` `line.png` `help.png` | Stills embedded in the README; regenerate with `scripts/record.sh --stills-only` |
| `demo.mp4` | Master screencast, 800x600; regenerate with `scripts/record.sh --video-only` |
| `demo.gif` | Native-resolution GIF embedded in the README; produced by `vhs scripts/demo.tape` |
| `palette.png` | The app's 16 render colours as an ffmpeg `paletteuse` palette; regenerate with `cd scripts && uv run --with pillow palette.py` |

## Usage

Regenerate everything with one command:

```sh
scripts/record.sh
```

It needs `cargo`, `uv`, `vhs`, `ttyd` and `ffmpeg` on your `PATH` (vhs drives
ttyd; ffmpeg is used for the artifact summary).

To build a custom GIF from the master with the palette, using sharp settings
(no dither, nearest-neighbour scale):

```sh
ffmpeg -i screenshots/demo.mp4 -i screenshots/palette.png -filter_complex \
  "[0:v]fps=8,scale=600:-1:flags=neighbor[l];[l][1:v]paletteuse=dither=none:diff_mode=rectangle" \
  -loop 0 out.gif
```

The tape (`scripts/demo.tape`) is the source of truth for the tour's scene
list.
