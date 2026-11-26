# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.3.4] - 2026-11-26

### Fixed

- Several herdr sessions at once. Each session runs its own server, but all
  share the plugin state dir, and workspace and pane ids (`w1`, `w1:p1`)
  repeat in every session. The sidebar tag caches were shared, so a sweep in
  one session could prune another's entries or mark a tag as already in
  place in a session where it was never written. The caches now live in one
  `state-<key>.json` per session, keyed by a hash of `HERDR_SOCKET_PATH`;
  `state.json` keeps only what belongs to the shared `config.toml` and the
  window tint. The old shared caches are ignored and dropped on the next
  save (one redundant tag write per pane and Space, once).
- Startup in several sessions at once no longer skips the theme. A held lock
  is retried with backoff for up to three seconds instead of printing
  "another apply is running; skipped". The theme write keeps a global lock,
  since `config.toml` is shared; sidebar tags take a per-session lock.
- Sidebar tags come back after a server restart. herdr restores the same ids
  without their reported tokens, so the cache is now dropped when the
  session's server instance changes.
- Sidebar rows replaced after the plugin wrote them are left alone. Once
  written, `agents`/`spaces` rows were treated as the plugin's forever, so
  rows set later by the user or another tool (for example a kit with its own
  `[ui.sidebar.agents]` rows) were overwritten on the next focus and deleted
  by `clear`. The plugin now records a fingerprint of the rows it wrote; rows
  that no longer match are released with a one-time "rows were changed by
  someone else; leaving them alone" and never rewritten or removed. Rows
  recorded by 0.3.3 without a fingerprint stay managed only if they equal
  what the plugin would generate (dark or light). Pane and Space tokens are
  still reported either way, so other rows can keep using `$sc_<palette>`.

- herdr's config, the plugin config dir and the state dir now follow
  `XDG_CONFIG_HOME` / `XDG_STATE_HOME` like herdr does when no explicit path
  is given. With only `XDG_CONFIG_HOME` set, `apply` failed reading
  `~/.config/herdr/config.toml`, and the CLI looked for the plugin config
  outside herdr's config dir. An empty XDG value counts as unset.

### Changed

- `status` shows the session, its socket and its state file. `clear` drops
  only the calling session's tags; the shared theme and rows are restored
  for all sessions as before.

## [0.3.3] - 2026-09-08

### Added

- Palette actions. Every palette is registered as a `Colour: <name>` action
  for workspaces and a `Pin: <name>` action for panes, plus a clear entry each,
  so a colour can be bound to a key (`type = "plugin_action"`) or invoked with
  `herdr plugin action invoke`. Workspace actions save the same persisted rule
  as `set`; pane actions pin the pane's agent (a toast explains when a pane has
  no agent to pin). herdr triggers plugin actions through keybindings, not its
  right-click menu. Custom palettes stay CLI-only, since plugin actions are
  static in the manifest. A `manifest-actions` subcommand regenerates them.

## [0.3.2] - 2026-09-08

### Added

- Prebuilt release binaries for macOS (arm64, x86_64) and Linux (x86_64,
  arm64). When `cargo` is absent the shim downloads the binary matching the
  host and the manifest version and verifies its SHA-256 before use, so
  installing the plugin no longer requires a Rust toolchain. With `cargo`
  present the plugin still builds from source, so `herdr plugin link` on a
  checkout reflects local edits.

## [0.3.1] - 2026-09-07

### Fixed

- Window tint is scoped to the herdr session the hook runs under. With two
  sessions open (for example `herdr` and `herdr --session demo`), a focus
  change in one no longer recolours the other session's window. The session
  of each tinted window is recorded in state so stale-window cleanup only
  touches the caller's own windows.

## [0.3.0] - 2026-09-07

### Added

- Window tint: the terminal window hosting the herdr client, title bar
  included, takes the focused workspace's colour. Apple Terminal via
  AppleScript on the tab whose tty runs the client; the first colour seen is
  saved in state and restored on `clear`, on a switch to the base theme, or
  when the client disappears. `[window] tint` toggles it; palettes accept an
  optional `window_bg` (defaults to `pane_bg`, then `sidebar_bg`).
- `status` reports the tinted windows.

## [0.2.0] - 2026-09-07

### Added

- Sidebar colouring: one coloured `$sc_<palette>` token per palette in
  `ui.sidebar.agents.rows` / `ui.sidebar.spaces.rows` (written only when the
  user has not set their own rows), with exactly the matching token reported
  on every pane and Space. `sweep` refreshes tags; it also runs on
  `workspace.created` and `pane.agent_status_changed`.
- Per-agent colour: `[[agents]]` rules keyed by agent session id;
  `set-agent` / `unset-agent` commands.
- Pane tint: `osc` prints an OSC 11 sequence for the calling pane; `shell-hook`
  prints or installs a guarded `~/.zshrc` hook; `install-cli` symlinks the
  tool into `~/.local/bin`.
- Commands with arguments: `set`, `unset`, `focus`, `palettes`.
- Light-theme support: `[palettes.<name>.light]` variants, chosen from
  `theme.name`; shipped palettes carry catppuccin-latte values.
- A write lock in the state dir, and a toast when a write is rolled back or a
  reload fails.

### Changed

- `status` lists every pane with its agent, workspace, palette and why.

## [0.1.1] - 2026-09-07

### Fixed

- `apply` always reconciles against `config.toml` instead of trusting cached
  state, so a `[theme.custom]` block removed by hand or by another tool is
  restored on the next event rather than after the next workspace switch.

## [0.1.0] - 2026-09-07

### Added

- Theme follows the focused workspace via a `workspace.focused` hook and
  `herdr server reload-config`.
- Explicit rules by workspace `label` or directory `path` (prefix, longest
  wins); automatic palette by directory hash for everything else.
- Eight catppuccin-tuned palettes.
- `apply`, `clear`, `status` plugin actions; `validate` and `--dry-run` from
  the CLI; `apply` as a startup hook.
- Safety contract: format-preserving edits, token allowlist and strict hex
  validation, one-time backup, `herdr config check` with rollback, atomic
  writes, `clear` restores the original bytes.
