# hypr-music-bg

> [!WARNING]
> **Work in progress. Not ready for general use, and not accepting issues yet.**
>
> The Issues tab is deliberately disabled. Config keys, the control protocol and
> the source chain are all still moving, and large parts have only ever run on
> one machine. Please don't build anything on this yet.
>
> If you found this and it looks useful: star it and come back. See
> [Project status](#project-status) for exactly what is and isn't verified.

Sets the currently playing track's album art as your wallpaper, on Hyprland and
other Wayland compositors.

Event-driven off MPRIS via D-Bus — no polling loop. Pulls art from a
configurable chain of sources with a resolution floor and match verification,
and renders per monitor.

## Project status

Verified end-to-end on real hardware:

- `mpris`, `local`, `coverartarchive` and `deezer` sources
- The resolution-floor and match-verification policy, including fallthrough
- DankMaterialShell as wallpaper backend
- Per-monitor and spanning render layouts across two 2560x1440 displays
- MPRIS event handling: album changes, pause, stop, resume
- The control socket and every CLI subcommand
- The system tray: registration, the full menu tree, and live state tracking
  (verified by introspecting dbusmenu over D-Bus)
- Cache eviction, including the rule that a wallpaper currently on screen is
  never evicted — measured against a real 126 MB cache
- The acceptance policy itself: fallthrough, degradation, match rejection,
  deferred candidates, and both cache short circuits

**Implemented but never executed against a live service** — no credentials, so
no real call has been made. Treat as unproven:

- `spotify`, `discogs`, `fanarttv`, `subsonic`, `itunes`, `exec`

Their URL construction, auth shape and response parsing are unit-tested, but
"compiles and parses a fixture" is not "works".

The tray's **album-art icon** is in the same category: the ARGB conversion has a
test, but no cover has actually been rendered into a bar yet.

Known gaps:

- Tested on exactly one configuration: Hyprland + DankMaterialShell, two
  1440p displays, Feishin against Navidrome. swww, hyprpaper and swaybg
  backends are implemented but unexercised.
- Internet radio and other thin-metadata sources are rejected rather than
  handled. Parsing `Artist - Title` out of stream titles is planned.
- No monitor hotplug handling for the captured original wallpaper.

Roadmap, roughly in order: a settings GUI, stream title parsing, packaging.

Resolution currently takes about 2.7 s cold and 0.4 s warm, of which roughly
2.3 s is network — a MusicBrainz query plus Cover Art Archive's redirect chain.
Not yet optimised.

## Why another one

Everything else in this space is either single-player (MPD only, Spotify only),
X11-only, or both. The handful of Wayland scripts that exist hardcode one art
source and take the first response they get.

That last part is the actual problem. Search-based catalogues do not reliably
signal "I don't have this" — they answer with something else. A live query to
the iTunes Search API for `D12 - Devil's Night` returns, as its top album
result:

```
artistName:     Ophélie Gaillard, Sandrine Piau & Pulcinella Orchestra
collectionName: Boccherini: Cello Concertos, Stabat Mater & Quintet
```

Full confidence, valid artwork, wrong universe. A "first response wins" chain
puts a cello record on your desktop. So every candidate here is checked against
the MPRIS metadata — the one thing known to be true — before it is accepted.

## How the chain works

Sources are tried in configured order. A candidate is accepted only if it
**verifies** (looks like the album that is playing) and **clears
`min_resolution`**. Otherwise the chain moves on.

1. **Preferred** — first source, in order, that verifies and meets the floor.
2. **Degraded** — nothing met the floor, so the largest thing that verified.
3. **Fallback** — nothing verified at all, so your configured static wallpaper.

This is what lets `mpris` sit first without costing quality. It answers
instantly and can never name the wrong album, but many servers only store a
small thumbnail. Real example on a Navidrome library:

```
mpris            300x300   below min_resolution 600, deferred
local            no candidates
coverartarchive  1200x1190  accepted
```

Four times the resolution, with the fast, always-correct source still tried
first.

## Sources

| Source | Key | Hops | Resolution | Notes |
|---|---|---|---|---|
| `mpris` | – | 0 | player-dependent | Instant, never the wrong album, often small |
| `local` | – | 0 | original | Files next to the audio, or `<root>/<artist>/<album>/` |
| `coverartarchive` | none | 2 | 1200, or originals at 1000–3000px | MusicBrainz-keyed, 1 req/sec |
| `deezer` | none | 1 | 1200 (measured cap) | Best zero-config remote |
| `itunes` | none | 1 | ~3000 | Returns wrong albums; needs `verify_match` |
| `spotify` | OAuth | 1 | 640 | Lower than keyless Deezer |
| `discogs` | token | 1 | ~600 | Strong release/pressing matching |
| `fanarttv` | key | 2 | 1920x1080 | Artist *backgrounds*, built as wallpapers |
| `subsonic` | password | 1 | your library's original | `size` only scales down |
| `exec` | – | – | – | Any command printing image paths |

Last.fm is deliberately absent: its API still responds, but serves a
placeholder star instead of artwork.

`fanarttv` is the odd one — its backgrounds are 16:9 and drawn to be
wallpapers, which suits a screen better than a zoomed square cover, but they
belong to the artist rather than the album and so will not change between
records by the same act.

## Install

```bash
cargo build --release
install -Dm755 target/release/hypr-music-bg ~/.local/bin/hypr-music-bg
```

```bash
mkdir -p ~/.config/hypr-music-bg && cp config.example.toml ~/.config/hypr-music-bg/config.toml
```

## Usage

```bash
hypr-music-bg about
```

Prints this binary's version, git commit and build time, the running daemon's,
and whether they match. Worth knowing because the crate version stays `0.1.0`
across every rebuild, so it cannot tell two builds apart — and a daemon started
before a rebuild keeps running the old code without announcing it:

```
this binary
  0.1.0 (91b626f) built 2026-08-02T18:18:15Z
running daemon
  0.1.0 (edd46a8) built 2026-07-31T00:25:13Z
MISMATCH: the daemon is running a different build. Restart it.
```

A commit ending `-dirty` was built from an uncommitted tree.

```bash
hypr-music-bg doctor
```

Prints detected monitors, the active player, the resolved wallpaper backend, the
source chain, and the same build comparison. Start here.

```bash
hypr-music-bg probe
```

Shows what the chain returns for the current track, and which source won,
without touching the wallpaper.

```bash
hypr-music-bg once --dry-run
```

Renders the wallpapers and prints their paths without applying them. Drop
`--dry-run` to apply once and exit.

```bash
hypr-music-bg run
```

The daemon. Follows the active player until interrupted, then restores the
wallpaper that was set when it started.

Set `HMB_LOG=debug` for per-source decisions, or change it live with
`hypr-music-bg log-level debug`.

### Controlling a running daemon

The daemon listens on `$XDG_RUNTIME_DIR/hypr-music-bg.sock`. These subcommands
talk to it, so they work from a Hyprland keybind or a script:

| Command | Effect |
|---|---|
| `status` | Everything it knows: track, art, chain outcome, cache size |
| `toggle` / `enable` / `disable` | Stop or resume reacting to the player without exiting |
| `refresh [--bypass-cache]` | Re-resolve the current album; `--bypass-cache` re-walks the chain |
| `restore` | Put back the pre-daemon wallpaper |
| `reload` | Re-read the config and rebuild the source chain |
| `clear-cache` | Delete cached art and renders (keeps the saved original wallpaper) |
| `log-level <level>` | Change verbosity live |
| `quit` | Restore and exit cleanly |

`status` is where the chain becomes visible, which is the fastest way to see why
a given album got the art it did:

```
chain (min 600px):
  mpris              300x300, below min 600
  local              no candidates
  coverartarchive    1200x1075 accepted
```

The wire format is line-delimited JSON, so it is drivable by hand:

```bash
echo '{"command":"status"}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/hypr-music-bg.sock
```

## Running it as a service

```ini
# ~/.config/systemd/user/hypr-music-bg.service
[Unit]
Description=Album art wallpaper daemon
PartOf=graphical-session.target
After=graphical-session.target

[Service]
Type=simple
ExecStart=%h/.local/bin/hypr-music-bg run
Restart=on-failure

[Install]
WantedBy=graphical-session.target
```

```bash
systemctl --user enable --now hypr-music-bg.service
```

## Colour theming

Off by default, and that is deliberate: recolouring GTK, your terminal and your
browser on every album change is a much larger intervention than setting a
wallpaper, and installing a wallpaper tool is not consent for it.

`theme.mode = "auto"` opts in and picks per setup. Shells that already theme
themselves from the wallpaper are left alone — DankMaterialShell binds its
scheme to the wallpaper it is handed, so setting one through it has *already*
re-derived the colours from the album art by the time we could act, and running
matugen ourselves would race it for the same template outputs. On swww,
hyprpaper or swaybg there is no such integration, so `auto` runs matugen, or
pywal if matugen is absent.

`source` picks what gets sampled: `cover` for purer colours from the artwork
itself, or `wallpaper` for the composed image actually on screen.

## Wallpaper backends

`auto` probes for what is actually running, in order: DankMaterialShell, swww,
hyprpaper, swaybg. Order matters — a shell that owns the background will
coexist happily with an installed-but-idle `swww` binary, and driving both
means they fight over the surface.

Anything else can be driven with `backend = "command"` and a `{image}` /
`{monitor}` template.

## Configuration

See [config.example.toml](config.example.toml), which documents every option.

Credentials are always referenced by environment variable name, never written
in the config, so a config file is safe to share.
