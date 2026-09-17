> [!NOTE]
> This project is entirely AI-generated. While care has been taken to ensure quality, users should review and verify the code for their specific use cases.

# walldeck

A wallpaper picker for Wayland. Opens as a layer-shell overlay, shows your
wallpapers as a scrollable ribbon of thumbnails, and hands the one you choose
to an `awww`-compatible backend.

It's a single static binary with four dependencies and no runtime helpers —
no external menu program, no image-processing subprocesses, no font files to
locate at runtime, no GPU context. Thumbnails
are decoded and cached in-process; the UI is a software blit into a shared
memory buffer.

```
┌────────────────────────────────────────────┐
│  ▢   ▢   ▢   ▢   ▢   ▢   ▢   ▢   ▢   ▢    │   ← thumbnail ribbon
└────────────────────────────────────────────┘
              ╭──────────────────╮
              │  SEARCH          │               ← filter by filename
              ╰──────────────────╯
```

## Features

- **Layer-shell overlay** on `wlr-layer-shell`, centered, keyboard-exclusive.
- **Smooth everything.** Scroll position and the selection ring are eased
  toward their targets every frame rather than snapping; touchpad and wheel
  momentum decay naturally; open and close run a real cubic-bezier scale-and-
  fade. Keyboard navigation glides through the same motion path as scrolling.
- **Antialiased rounding** on the panel, the search bar, every thumbnail, and
  the selection ring, computed from a signed distance field rather than a
  hard in/out mask.
- **Search bar** that filters by filename as you type.
- **Two-target support.** Apply the same pick to a second namespace using a
  differently-suffixed file — typically a pre-blurred copy for an overview or
  exposé layer. Optional, and a missing second daemon is tolerated.
- **Thumbnail cache** that regenerates on source change and prunes orphans.
- **One config file**, every key optional, with warnings instead of failures.

## Requirements

- A Wayland compositor implementing `wlr-layer-shell`.
- A wallpaper daemon with a `<command> img [flags] <path>` interface —
  [`awww`](https://codeberg.org/LGFae/awww) (formerly `swww`, whose GitHub
  repo is archived) or a fork of it. The secondary-target feature needs
  `--namespace`, which `awww` has and pre-rename `swww` does not.
- `libxkbcommon` (for keyboard handling).
- Optional: `notify-send` for notifications, and a CLI audio player such as
  `pw-play` for the completion sound. Both are skipped if absent.

## Install

```sh
cargo build --release
install -Dm755 target/release/walldeck ~/.local/bin/walldeck
install -Dm644 config.example.conf ~/.config/walldeck/config.conf
```

Then bind it to a key in your compositor's config. It's a one-shot program:
it opens, waits for a choice, applies it, and exits.

> **Build in release mode.** The renderer is pure software compositing and
> the animation timing assumes roughly frame-paced ticks. A debug build is
> slow enough to visibly stutter and to throw the easing math off.

## Usage

```
walldeck [OPTIONS]

  -h, --help         Print help and exit
  -V, --version      Print version and exit
      --config-path  Print the config file path in use and exit
```

### Controls

| Key | Action |
| --- | --- |
| `←` / `→` | Move the selection (loops past either end) |
| `Home` / `End` | Jump to first / last |
| `Enter` | Apply the selection |
| Typing | Filter by filename |
| `Backspace` | Delete a search character |
| `Esc` | Clear the search, or close if it's already empty |
| Scroll | Pan the ribbon |
| Click | Apply that thumbnail |

## Configuration

walldeck reads, in order of preference:

1. `$WALLDECK_CONFIG`
2. `$XDG_CONFIG_HOME/walldeck/config.conf`
3. `~/.config/walldeck/config.conf`

Every key is optional and falls back to a built-in default, so you only need
to write the lines you want to change. Unknown keys and unparsable values
warn on stderr and are otherwise ignored — a typo never stops the picker from
opening.

See [`config.example.conf`](config.example.conf) for the full annotated set.
The shape of it:

```ini
[paths]
wallpaper_dir  = ~/Pictures/Wallpapers
cache_dir      = $XDG_CACHE_HOME/walldeck
thumbnail_size = 350
sound_file     = /usr/share/sounds/freedesktop/stereo/complete.oga
sound_command  = pw-play
notifications  = true

[wallpaper]
command             = awww
namespace           =
transition_type     = center
transition_fps      = 60
extra_args          =

[secondary]
secondary_enabled    = true
secondary_namespace  = overview
secondary_suffix     = -b
secondary_required   = false
secondary_auto_blur  = false
secondary_blur_sigma = 20.0

[layout]
window_width = 1179
icon_size    = 350
# …

[colors]
background = "#1e1e2eeb"
# …

[animation]
open_close_curve = 0.42, 0.0, 0.58, 1.0
# …
```

`[section]` headers are cosmetic grouping only — keys are global, so don't
repeat a key under a different section.

### Transitions

Every `transition_*`, `resize`, `fill_color`, and `filter` value is passed
straight through to the backend as the matching `--flag value` pair, and is
**omitted entirely when left empty**. That means blanks mean "use whatever
the backend defaults to", and walldeck doesn't need to know or care which
transition types your particular backend supports — check its `img --help`
and put the value here.

Anything not modelled as its own key goes in `extra_args`, appended verbatim:

```ini
transition_type = wipe
transition_angle = 30
extra_args = --invert-y
```

`extra_args` is whitespace-split with no quoting or escaping, so arguments
containing spaces aren't expressible. Wrap walldeck in a script if you need
that.

### Two targets

`[secondary]` applies your pick a second time, to a different namespace,
using a file with `secondary_suffix` appended to the stem — so picking
`forest.jpg` also applies `forest-b.jpg` to the `overview` namespace. Files
ending in that suffix are hidden from the picker, so the variants don't show
up as pickable wallpapers themselves.

Each target has its own independent transition settings (prefix the key with
`secondary_`), so the two can animate differently.

**If you only run one daemon, set `secondary_enabled = false`.** Left
enabled, walldeck tolerates the second target failing: with
`secondary_required = false` (the default), a missing daemon or a missing
variant file produces a warning while the primary wallpaper still applies and
the program still exits `0`. Set `secondary_required = true` to make those
hard failures instead.

**No `-b` file for a given wallpaper?** Set `secondary_auto_blur = true` and
walldeck generates one instead of leaving the target unset — a Gaussian blur
of the primary image, strength controlled by `secondary_blur_sigma` (higher
is blurrier). Generated copies are cached under `cache_dir/blurred`,
regenerated only when the source wallpaper changes, and a real `-b` file you
add later always takes priority over — and replaces — a generated one.

### Thumbnail cache

Cached thumbnails are square cover-crops, one file per wallpaper, named after
the wallpaper and stored in `cache_dir`. They're regenerated when the source
file is newer than its thumbnail, and deleted when the source disappears, so
the cache stays in step on its own. Deleting the whole directory is always
safe; it rebuilds on the next run.

Keep `thumbnail_size` at or above `icon_size`, or the picker will be
upscaling cached thumbnails to fill the cells.

## Design notes

**Render pacing.** `draw()` is the only place that attaches a buffer,
requests a frame callback, and commits, and a `frame_pending` flag guarantees
at most one buffer in flight. Input handlers never draw — they update state
and mark the surface dirty, and either the immediate draw or the next frame
callback picks the change up. Drawing straight from input events instead
submits buffers faster than the compositor can display them under touchpad
bursts, which shows up as tearing.

**Motion.** Scroll and highlight positions are each a logical `target` set
instantly by input plus a rendered `value` that chases it exponentially every
frame. Momentum nudges the *target* after input stops; the rendered value
just trails it. Keyboard navigation drives the same mechanism, which is why
arrow keys glide rather than jump.

**Antialiasing.** Rounded corners come from a rounded-box signed distance
field: a pixel's alpha is its fractional coverage by the shape, so a pixel
40% inside the curve contributes 40% alpha. One function covers corners,
straight edges, inside, and outside, so nothing special-cases quadrants.
Coverage is verified against 16×16 supersampled ground truth in the tests.

**Premultiplied compositing.** The canvas is premultiplied BGRA, matching
what a `wl_shm` `Argb8888` buffer is defined to hold. Antialiasing depends on
this: a half-covered pixel needs half intensity *and* half alpha, or partial
edges glow against whatever is behind them.

**No GPU path.** Everything is a software blit into `wl_shm` buffers. That's
a deliberate trade for a picker that's on screen for a couple of seconds at a
time and would otherwise need an EGL context to draw a grid of squares.

**Compositing math is fixed-point, not float.** `blend_over()` — the "over"
composite every covered pixel of the panel, the pill, every thumbnail, and
the ring goes through, once per frame — uses integer math with an `a >= 255`
fast path that skips blending entirely for opaque interior pixels, rather
than the more obvious per-channel `f64` version. Since the eased motion means
a full repaint every frame rather than only on change, this loop is the
actual hot path: the straightforward float version measured at roughly a
42fps ceiling for one frame's worth of compositing on typical hardware,
against ~108fps for the integer version.

**Minimal dependencies.** Config parsing is a hand-rolled `key = value`
reader rather than `serde`/`toml`, and the search bar uses a built-in 5×7
bitmap font rather than a rasterizer hunting for a system font file at
runtime. Both are a few dozen lines and remove a dependency tree apiece.

## Tests

```sh
cargo test
```

Covers config parsing and path expansion, filename/variant handling, and the
antialiasing geometry (against supersampled ground truth) and blend math.
The Wayland event loop isn't covered — it needs a live compositor.

## License

MIT. See [LICENSE](LICENSE).
