# Design decisions

Why this project is shaped the way it is. Every decision below came from a
measurement or a failing test, and the evidence is recorded with it — so a
future change knows what it would be overturning.

---

## The source chain is not "first response wins"

**Decision.** Art is resolved by walking a configured chain of sources. A
candidate is accepted only if it passes *two* gates: it must verify against the
MPRIS metadata, and it must clear `min_resolution`.

**Why.** Search-based catalogues do not reliably signal "I don't have this" —
they answer with something else. A live query to the iTunes Search API for
`D12 - Devil's Night` returned, as its top album result:

```
artistName:     Ophélie Gaillard, Sandrine Piau & Pulcinella Orchestra
collectionName: Boccherini: Cello Concertos, Stabat Mater & Quintet
```

Full confidence, valid artwork, wrong universe. Every existing tool in this
space takes the first non-empty response, which puts a cello record on the
desktop. Verification against the MPRIS metadata — the one thing known to be
true — is what makes a multi-source chain safe.

**Consequence.** Sources that search must attach a `Claim` (what they think the
image depicts). Sources that read what the player or filesystem already has
(`mpris`, `local`, `exec`) carry no claim and skip verification, because they
are structurally incapable of naming the wrong album.

---

## Three acceptance tiers, not one

1. **Preferred** — first source, in order, that verifies and meets the floor.
2. **Degraded** — nothing met the floor, so the largest candidate that verified.
3. **Fallback** — nothing verified, so a configured static wallpaper.

**Why.** A single "accept or reject" rule forces a choice between never
degrading (blank wallpapers on obscure albums) and always accepting (300px art
stretched to 1440p). The middle tier is flagged `degraded` so callers can warn
rather than silently deliver something poor.

---

## `mpris` leads the chain, and the resolution floor is what protects quality

**Decision.** The default order is `mpris → local → coverartarchive → deezer`.

**Why.** `mpris` answers instantly, needs no network for local files, and can
never name the wrong album. Its weakness is resolution. Measured against a real
Navidrome library via Feishin:

```
size=300  → 300x300 PNG (117530 bytes)
size=600  → 300x300 PNG (117530 bytes)   byte-identical
size=1200 → 300x300 PNG (117530 bytes)
no size   → 300x300 PNG (117530 bytes)
```

Subsonic's `size` parameter only ever scales *down*. For the same album, Cover
Art Archive served 1200×1190. So the intuitive ordering — own library first,
internet as fallback — would systematically deliver the *worst* art available.

The floor resolves the tension: correctness-first ordering, resolution-first
outcomes. `mpris` is tried first and falls through on its own when too small.

---

## Source measurements

All figures below were measured against the live APIs, not taken from
documentation.

| Source | Key | Hops | Measured | Note |
|---|---|---|---|---|
| Deezer | none | 1 | **1200** | Docs say ~1000; CDN re-renders from the URL and caps at 1200. Requesting 1500 returns 1200, so `size` is clamped rather than promising pixels that never arrive. |
| Cover Art Archive | none | 2 | **1200×1190** | Two hops via MusicBrainz, rate-limited to 1 req/sec. `size = 0` fetches the original upload (often 1000–3000px). |
| iTunes | none | 1 | ~3000 | Resizable by rewriting the path. Returns wrong albums confidently — see above. |
| Spotify | OAuth | 1 | 640 | *Lower* than keyless Deezer. Useful as a match cross-check, not for resolution. |
| Last.fm | key | 1 | **unusable** | API still responds but serves a placeholder star instead of artwork. Deliberately not implemented. |

---

## MPRIS is a standard, not per-player integration

**Decision.** There is zero player-specific code. Everything comes from the
freedesktop MPRIS spec over D-Bus.

**But adherence is uneven.** Observed on one real session:

| Player | artist | albumArtist | album | title |
|---|---|---|---|---|
| Feishin | `as` array | `as` array | `s` | `s` |
| playerctld | `as` array | **absent** | `s` | `s` |
| wlir_radio | `as` array | **absent** | `s` | `s` |
| chromium | **absent** | **absent** | **absent** | **absent** |

Three consequences, each load-bearing:

1. The spec types `xesam:artist` as `as` (array), but players send bare strings.
   `as_string()` accepts both.
2. `albumArtist` was absent on three of four players, so `search_artist()`'s
   fallback to `xesam:artist` is not a nicety. It matters most on compilations,
   where the track performer will not find the album.
3. Chromium exposes only `mpris:artUrl` with no text metadata — nothing to
   search with. Browsers are in the default ignore list.

**`playerctld` is ignored by default.** It is playerctl's proxy daemon and
mirrors whatever *it* considers active, using its own notion of "active". During
testing it mirrored a paused radio stream while a different player was actually
playing. Following it means second-hand metadata and the same track appearing
twice on the bus, with the winner depending on `ListNames` ordering.

---

## Thin metadata is gated before it reaches a catalogue

**Decision.** `TrackInfo::is_searchable()` requires a non-empty artist *and*
album. Sources that search are skipped when it fails.

**Why.** Internet radio reports an empty artist with the station name in the
album field. That passes the basic "is anything set" guard, so without this gate
the daemon spends a rate-limited MusicBrainz lookup on an album that does not
exist and then writes a negative-cache entry, poisoning it even after better
metadata arrives.

**Extension point.** Stream title parsing (`Artist - Title` out of
`xesam:title`) belongs *before* this gate, normalising a stream's fields so the
gate then passes — not special-cased inside every source.

---

## A wallpaper currently on screen is never evicted

**Decision.** Cache pruning excludes any render recorded in `status.rendered`,
and runs *after* new renders are applied.

**Why.** Renders dominate the cache — measured at 116 MB of 126 MB on a real
install, roughly 3 MB per album on two 1440p monitors. So a size limit reaches
for them first. But the compositor holds the *path*, not a copy: DMS and swww
both do. Deleting a live render does not cost a re-render, it breaks the
wallpaper that is up.

**Consequence.** The total can sit slightly above the configured budget, because
protected files cannot be reclaimed. A 20 MiB test settled at 22 MiB. That is
correct, not a leak.

`original.json` is likewise never touched: it is the only record of the
pre-daemon wallpaper, and losing it makes restore permanently impossible.

---

## The daemon refuses to record its own renders as "the original"

**Decision.** At startup the daemon captures the current wallpaper, but rejects
any path inside its own cache, and persists the result.

**Why.** After any previous run, querying the backend returns *our* cover as the
current wallpaper. Recording that as the original means restore replays a stale
album cover forever, and the real wallpaper is lost after the first run. This was
observed live before it was fixed.

---

## Colour theming defaults to off

**Decision.** `theme.mode = "off"`. `auto` is opt-in.

**Why.** Recolouring GTK, the terminal and the browser on every album change is a
far larger intervention than setting a wallpaper. Nobody installing a wallpaper
tool has implicitly consented to it.

**And `auto` declines where the work is already done.** DankMaterialShell binds
its theme to `SessionData.monitorWallpapers`, which is exactly what
`dms ipc call wallpaper setFor` writes — verified by reading `Theme.qml`, where
`wallpaperPath` is a QML binding over the wallpaper and
`setDesiredTheme("image", …)` shells out to matugen. A wallpaper set through DMS
has already re-derived the entire scheme before we could act. Running matugen
ourselves would be a second pass racing the first for the same template files.

That decision is made from the **wallpaper backend**, not by reading the shell's
own settings: a misread would cause a double run, whereas doing nothing is
harmless. The failure mode was chosen deliberately.

---

## Fallback wallpapers are rendered plainly, not as covers

**Decision.** A fallback image gets `Fill` with no blur and no darkening, and its
layout is inferred from aspect ratio — wider than 2.5:1 is sliced across
monitors.

**Why.** Album art is square and wants the blur-backdrop treatment. A wallpaper
is already screen-shaped; blurring it and centring a shrunken copy of itself is
nonsense. This was a real bug: 5120×1440 spanning wallpapers were being composed
as if they were covers.

---

## The control surface is a socket, and `Status` is a wire format

**Decision.** A Unix socket in `$XDG_RUNTIME_DIR` carries line-delimited JSON.
The tray, the CLI and the GUI all go through the same `Controller`.

**Why line-delimited JSON.** So it can be driven by hand:

```bash
echo '{"command":"status"}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/hypr-music-bg.sock
```

**`serde(default)` on the status types is load-bearing.** The daemon keeps
running while `cargo build` replaces the binary the CLI invokes, so a newer
client talking to an older daemon is the normal case. Without it, adding one
field breaks every client command against a daemon that predates it. Observed:

```
$ hypr-music-bg status
Error: missing field `theme_mode` at line 1 column 375
```

**Socket paths are length-checked up front.** `sockaddr_un` caps the path near
108 bytes, and exceeding it produced a bind failure whose cause was swallowed.

---

## The chain walk is reported, not just the winner

`Status.chain` records what every source did. The whole design is a fallback
chain, so "which source won" is far less useful for diagnosis than:

```
mpris              300x300, below min 600
local              no candidates
coverartarchive    1200x1075 accepted
```

Surfaced in `status`, `probe`, and the tray.

---

## Build identity is stamped, because the version cannot distinguish builds

**Decision.** A build script stamps the git commit (with `-dirty`), branch and
UTC build time. `about` compares this binary against the running daemon.

**Why.** The crate version stays `0.1.0` across every rebuild. A daemon started
before a rebuild keeps running old code with nothing announcing it. This cost
time three separate times in one session, most memorably with an autostart entry
pointing at a stale `~/.local/bin` copy while every rebuild landed in
`target/release`.

**Comparison lives in one function** because two call sites disagreed while each
had its own copy: `doctor` called a client and daemon launched from the *same
binary* a mismatch, purely because the tree was dirty. Same file and same build
instant now counts as Same regardless of commit; a dirty tree is Indeterminate
rather than a false alarm.

---

## The tray is StatusNotifierItem, and two protocol constraints shape it

Wayland has no XEmbed tray, so this speaks `StatusNotifierItem` plus
`com.canonical.dbusmenu`, via `ksni`.

1. **`Tray` methods are synchronous.** `Status` therefore lives behind a
   `std::sync::RwLock`, not a tokio one. Nothing holds that guard across an
   await, so the std lock was always correct.
2. **`activate` callbacks must not block** or the menu freezes. Every action
   hands work to the runtime and returns immediately.

**dbusmenu has no text entry and no sliders.** That is why numeric and list
settings are not editable there — the menu offers what a menu can express, and
defers the rest to the settings GUI.

**The refresh signal covers every mutation of `Status`, not just artwork.**
dbusmenu clients cache the layout until its revision changes, so a narrower
signal left the menu stale — toggling from the CLI reported success while the
checkmark stayed put.

---

## The settings GUI is a separate binary, and edits are surgical

**Decision.** `hypr-music-bg-settings` behind `--features gui`. `config`,
`control`, `build_info` and `config_edit` moved into a shared library.

**Why a library.** Two binaries cannot share a module by declaring it twice —
that produces two unrelated types with the same name, so a `Status` decoded by
one would not be the `Status` the other defined. The daemon's internals stay in
the binary; the GUI has no use for art sources or rendering.

**Why `toml_edit`.** Serialising a `Config` and writing it back deletes every
comment — 128 in the shipped example, which is where most of the explanation of
what the settings *mean* lives.

**A test caught this doing the very thing it was meant to prevent.** Replacing a
`toml_edit::Item` discards the *decor*, and a trailing comment lives in the
decor — so `set()` was eating the comment beside any value it touched. The old
decor is now carried across.

**Source reordering carries each entry's own keys.** Dropping them would silently
reset sizes and credential references, and the chain is the one part of this
config that is laborious to reconstruct by hand.

**Saves are atomic, skipped when unchanged, and refuse to write TOML that would
not parse back.** A GUI bug should not leave a config the daemon cannot load.

---

## Credentials are referenced, never stored

Every credential-backed source names an *environment variable*, never the secret.
A config file is therefore safe to paste into an issue. The GUI does not edit
them at all.

This matters concretely here: a Subsonic `mpris:artUrl` carries the auth token in
its query string, so `doctor` redacts any remote artUrl rather than printing it.

---

## Event-driven, not polling

Every existing tool in this space busy-loops on `playerctl metadata`. This
subscribes to D-Bus `PropertiesChanged`.

**Both `Metadata` and `PlaybackStatus` are subscribed.** That is not redundant:
`PropertiesChanged` carries only the properties that actually changed, so a pause
emits `PlaybackStatus` with no `Metadata` at all. Listening on `Metadata` alone
made pause and stop invisible, which silently disabled `restore_on_stop`.

**Album changes are keyed on the album, not the track**, so the wallpaper does
not re-render between songs on one record.

**Resuming re-applies even when the album is unchanged.** Otherwise play → stop
(wallpaper restored) → resume leaves the restored wallpaper up for the rest of
the record.
