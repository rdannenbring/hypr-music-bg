# Tasks

Status as of `main` at `ea1fe90` (PRs #1 and #2 merged).

Design rationale for anything below lives in [DESIGN.md](DESIGN.md).

---

## Done

### Core
- MPRIS listener, event-driven off D-Bus `PropertiesChanged`
- Album-keyed change detection; pause, stop and resume handled
- Player preference and ignore lists; `playerctld` ignored by default
- Metadata quality gate before any catalogue lookup

### Art resolution
- Nine sources: `mpris`, `local`, `coverartarchive`, `deezer`, `itunes`,
  `spotify`, `discogs`, `fanarttv`, `subsonic`, plus an `exec` escape hatch
- Three-tier acceptance policy: preferred, degraded, fallback
- Match verification against MPRIS metadata
- Per-source chain outcome reporting
- Lookup cache, negative cache, and cache eviction by size and age

### Rendering
- Blur-backdrop, fill and fit styles
- Per-monitor and spanning layouts
- Plain rendering for fallback wallpapers, with layout inferred from aspect ratio

### Wallpaper backends
- DankMaterialShell, swww, hyprpaper, swaybg, arbitrary command
- `auto` detection
- Original wallpaper captured, persisted, and never overwritten with our own
  renders

### Interfaces
- Control socket, line-delimited JSON, in `$XDG_RUNTIME_DIR`
- CLI: `run`, `once`, `probe`, `doctor`, `about`, `status`, `toggle`, `enable`,
  `disable`, `refresh`, `restore`, `reload`, `clear-cache`, `log-level`, `quit`
- System tray: StatusNotifierItem + dbusmenu, with live state tracking
- Settings GUI, separate binary behind `--features gui`
- Comment-preserving config editing, including source-chain reordering
- Build stamping and mismatch detection

### Project
- MIT licence, public repo, Issues deliberately disabled
- README with an honest verified / unverified split
- 92 tests, clippy clean

---

## Verified on real hardware

- `mpris`, `local`, `coverartarchive`, `deezer`
- The acceptance policy, including fallthrough and degradation
- DankMaterialShell backend
- Per-monitor and spanning layouts on two 2560×1440 displays
- Album change, pause, stop, resume
- Control socket and every CLI subcommand
- Tray registration and the full menu tree, introspected over dbusmenu
- Cache eviction against a copy of a real 126 MB cache
- Config editing that preserves comments

---

## Unverified

Ranked by how likely they are to bite. **This is the top of the work queue.**

1. ~~**Tray album-art icon.**~~ **VERIFIED WORKING** — the cover renders in the
   bar. ARGB channel order is correct.
2. **Six credential-backed sources** — `spotify`, `discogs`, `fanarttv`,
   `subsonic`, `itunes`, `exec`. URL construction, auth shape and response
   parsing are unit-tested; no live call has ever been made.
3. **`swww`, `hyprpaper`, `swaybg` backends.** Implemented, never exercised.
4. **Settings GUI in real use.** It launches, renders, and leaves the config
   untouched on open — but no setting has been changed through it and saved on a
   live system.

---

## Pending

### Next
- [x] Merge the branch stack into `main` — done, PRs #1 and #2
- [x] Verify the tray icon renders — working
- [ ] Install the current build so the running daemon matches the source
- [ ] Verify the six credential-backed sources against live services
- [ ] Verify `swww` / `hyprpaper` / `swaybg` backends
- [ ] Change a setting in the GUI, save, confirm the daemon picks it up

### Roadmap
- [ ] **Stream title parsing** — derive artist and album from `Artist - Title`
      in `xesam:title`. Must run *before* `is_searchable()`, so a normalised
      stream passes the existing gate rather than each source special-casing it.
- [ ] **Packaging** — PKGBUILD, and a systemd unit installed rather than an
      autostart `.desktop` (which also makes the tray's Restart work, since it
      needs `INVOCATION_ID`)
- [ ] **Resolve latency** — 2740 ms cold, 417 ms warm. About 2.3 s is network: a
      MusicBrainz query plus Cover Art Archive's redirect chain through
      archive.org. Levers: put Deezer ahead of CAA (one hop to a fast CDN versus
      two hops plus redirects), or query sources concurrently.
- [ ] **Monitor hotplug** — originals are captured at startup only, so a display
      added later has nothing to restore to.
- [ ] **CHANGELOG** — worth starting at v0.1.0, not before.

### Deferred
- Persisting render style and layout changed from the tray. Session-only today;
  dbusmenu cannot express the alternative, so this belongs to the GUI.
- Other tray-adjacent ideas, parked as future-release items.

---

## Branch state

All merged. `main` at `ea1fe90`, no feature branches, no open PRs.

- PR #1 (`feat/theming`) — tray, cache eviction, resolver tests, theming,
  version-skew fix
- PR #2 (`feat/settings-gui`) — build stamping, tray About, settings window, docs

---

## Known traps

Recorded because each cost real time.

- **Stale binaries.** `~/.local/bin/hypr-music-bg` drifts from
  `target/release`. Run `hypr-music-bg about` to compare, or symlink while
  developing.
- **`path` is a reserved variable in zsh.** Using it as a loop variable destroys
  `$PATH` mid-script.
- **`ls` is aliased to `lsd`** on this machine and lists `.` and `..`, so naive
  file counts read two higher than reality.
- **Unix socket paths cap near 108 bytes.** Long `XDG_RUNTIME_DIR` values fail to
  bind; now checked up front with an explanatory message.
