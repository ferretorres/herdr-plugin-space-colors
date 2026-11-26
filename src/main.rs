//! herdr-space-colors — Peacock-style per-workspace colours for Herdr.
//!
//! Herdr's theme is global to a session and its socket API has no per-workspace
//! colour method. Three documented mechanisms, combined, give the effect:
//!
//!   1. Theme follows focus. Exactly one workspace is focused at a time, so on
//!      `workspace.focused` the plugin rewrites only the `[theme.custom]` keys
//!      it manages and asks the server to reload, which applies `theme` live.
//!   2. Sidebar rows. Row styling is fixed per token position, but which
//!      custom `$token` has a value is dynamic per pane/workspace. One token
//!      per palette, each styled in that palette's colour, and every pane and
//!      Space is given exactly the one matching its colour.
//!   3. Pane tint. Herdr honours OSC 11 (default background) per pane, and
//!      every pane — agent panes included — starts inside a login shell. A
//!      one-line shell hook runs `herdr-space-colors osc` so each pane paints
//!      itself at creation and after restarts.
//!
//! Safety contract, because this edits a file the user hand-maintains:
//!   * format-preserving edits via toml_edit — comments and ordering survive
//!   * only tokens from the `theme.custom` allowlist, only strict hex colours
//!   * sidebar rows are written only when the user has not set their own
//!   * a one-time backup next to config.toml before the first write
//!   * `herdr config check` after every write; failure restores the old bytes
//!   * `clear` removes exactly what this plugin wrote and nothing else

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
use toml_edit::{value, Array, DocumentMut, InlineTable, Item, Table, Value as TomlValue};

mod window;

const PLUGIN_ID: &str = "ferretorres.space-colors";
const TAG: &str = "space-colors";
const SOURCE: &str = "space-colors";
const TOKEN_PREFIX: &str = "sc_";
const DEFAULT_CONFIG: &str = include_str!("../config.example.toml");
const BACKUP_SUFFIX: &str = ".space-colors.bak";
const LOCK_STALE: Duration = Duration::from_secs(15);
/// How long a command waits for a lock held by another invocation.
const LOCK_WAIT: Duration = Duration::from_secs(3);
const HOOK_MARKER: &str = "# herdr-space-colors shell hook";

/// `theme.custom.*` tokens accepted by herdr 0.8.2. `herdr config check` does
/// not reject unknown tokens or malformed colours, so this list is the guard.
const THEME_TOKENS: &[&str] = &[
    "accent",
    "panel_bg",
    "sidebar_bg",
    "active_row_bg",
    "selection_bg",
    "surface0",
    "surface1",
    "surface_dim",
    "overlay0",
    "overlay1",
    "text",
    "subtext0",
    "mauve",
    "green",
    "yellow",
    "red",
    "blue",
    "teal",
    "peach",
];

/// Substrings of built-in theme names that render on a light background.
const LIGHT_THEME_HINTS: &[&str] = &["latte", "light", "dawn", "day", "lotus", "paper"];

type Res<T> = Result<T, String>;
type Tokens = BTreeMap<String, String>;

/// stdout writers that ignore a closed pipe (`status | head`) instead of
/// panicking the way `println!` does on EPIPE.
macro_rules! outln {
    () => {{ use std::io::Write as _; let _ = writeln!(std::io::stdout().lock()); }};
    ($($arg:tt)*) => {{ use std::io::Write as _; let _ = writeln!(std::io::stdout().lock(), $($arg)*); }};
}
macro_rules! out {
    ($($arg:tt)*) => {{ use std::io::Write as _; let _ = write!(std::io::stdout().lock(), $($arg)*); }};
}

// --- cli ------------------------------------------------------------------------

struct Args {
    cmd: String,
    positional: Vec<String>,
    dry_run: bool,
    write: bool,
    reset: bool,
    pane: Option<String>,
}

fn parse_args() -> Args {
    let mut it = env::args().skip(1);
    let mut a = Args {
        cmd: String::new(),
        positional: Vec::new(),
        dry_run: false,
        write: false,
        reset: false,
        pane: None,
    };
    while let Some(x) = it.next() {
        match x.as_str() {
            "--dry-run" => a.dry_run = true,
            "--write" => a.write = true,
            "--reset" => a.reset = true,
            "--pane" => a.pane = it.next(),
            "--current" => a.pane = env::var("HERDR_PANE_ID").ok(),
            _ if a.cmd.is_empty() => a.cmd = x,
            _ => a.positional.push(x),
        }
    }
    if a.cmd.is_empty() {
        a.cmd = "apply".into();
    }
    a
}

fn main() -> ExitCode {
    let a = parse_args();
    let result = match a.cmd.as_str() {
        "apply" => cmd_apply(a.dry_run),
        "sweep" => cmd_sweep(a.dry_run),
        "clear" => cmd_clear(a.dry_run),
        "status" => cmd_status(),
        "validate" => cmd_validate(),
        "palettes" => cmd_palettes(),
        "set" => cmd_set(&a.positional),
        "unset" => cmd_unset(&a.positional),
        "set-agent" => cmd_set_agent(&a.positional),
        "unset-agent" => cmd_unset_agent(&a.positional),
        "action-colour" => cmd_action_colour(&a.positional),
        "manifest-actions" => cmd_manifest_actions(),
        "focus" => cmd_focus(&a.positional),
        "osc" => cmd_osc(a.pane.as_deref(), a.reset),
        "install-cli" => cmd_install_cli(),
        "shell-hook" => cmd_shell_hook(a.write),
        "help" | "-h" | "--help" => {
            out!("{}", usage());
            Ok(())
        }
        other => Err(format!("unknown command `{other}`\n\n{}", usage())),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[{TAG}] {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> String {
    format!(
"herdr-space-colors — the Herdr theme, sidebar rows and panes follow each workspace's colour

Usage:
  herdr-space-colors [apply] [--dry-run]     colour the focused workspace + refresh sidebar tags (default)
  herdr-space-colors sweep   [--dry-run]     refresh sidebar tags only
  herdr-space-colors clear   [--dry-run]     remove everything this plugin wrote
  herdr-space-colors status                  what every workspace and pane gets, and why
  herdr-space-colors validate                check the plugin config and exit
  herdr-space-colors palettes                list palettes

  herdr-space-colors set <label|id|path> <palette>     pin a workspace (persisted rule)
  herdr-space-colors unset <label|path>
  herdr-space-colors set-agent <pane-id|session> <palette>   pin one agent session
  herdr-space-colors unset-agent <pane-id|session>
  herdr-space-colors focus <label|id>                  focus a workspace by label

  herdr-space-colors osc [--pane ID|--current] [--reset]   print the OSC 11 sequence for a pane
  herdr-space-colors install-cli              symlink this tool into ~/.local/bin
  herdr-space-colors shell-hook [--write]     print (or append to ~/.zshrc) the pane-tint hook

Plugin config: $HERDR_PLUGIN_CONFIG_DIR/config.toml  (seeded on first run)
Herdr config:  $HERDR_CONFIG_PATH, else `herdr_config` in the plugin config,
               else ${{XDG_CONFIG_HOME:-~/.config}}/herdr/config.toml
Backup:        <herdr config>{BACKUP_SUFFIX}, written once before the first edit
")
}

// --- environment ----------------------------------------------------------------

struct Ctx {
    herdr: String,
    config_dir: PathBuf,
    state_dir: PathBuf,
}

fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn ctx() -> Ctx {
    let herdr = env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    // Outside a herdr-launched command the plugin dirs are not injected; mirror
    // where herdr keeps them so both paths agree.
    let config_dir = env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| herdr_config_dir().join("plugins/config").join(PLUGIN_ID));
    // herdr keeps plugin state under the XDG state dir, not beside the config.
    let state_dir = env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| herdr_state_dir().join("plugins").join(PLUGIN_ID));
    Ctx {
        herdr,
        config_dir,
        state_dir,
    }
}

fn herdr_config_path(cfg: &PluginConfig) -> PathBuf {
    if let Some(p) = env::var_os("HERDR_CONFIG_PATH") {
        return PathBuf::from(p);
    }
    if let Some(p) = &cfg.herdr_config {
        return expand_home(p);
    }
    herdr_config_dir().join("config.toml")
}

/// herdr's directory rule: `$XDG_*_HOME/herdr` when set, else
/// `$HOME/<home_rel>/herdr`. An empty XDG value counts as unset, per the XDG
/// spec (herdr itself would resolve it to a relative `herdr`).
fn herdr_dir(xdg: Option<&std::ffi::OsStr>, home: &Path, home_rel: &str) -> PathBuf {
    match xdg.filter(|v| !v.is_empty()) {
        Some(base) => PathBuf::from(base).join("herdr"),
        None => home.join(home_rel).join("herdr"),
    }
}

fn herdr_config_dir() -> PathBuf {
    herdr_dir(
        env::var_os("XDG_CONFIG_HOME").as_deref(),
        &home(),
        ".config",
    )
}

fn herdr_state_dir() -> PathBuf {
    herdr_dir(
        env::var_os("XDG_STATE_HOME").as_deref(),
        &home(),
        ".local/state",
    )
}

fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if p == "~" => home(),
        None => PathBuf::from(p),
    }
}

fn plugin_root() -> Option<PathBuf> {
    if let Some(r) = env::var_os("HERDR_PLUGIN_ROOT") {
        return Some(PathBuf::from(r));
    }
    // target/release/<bin> → plugin root
    env::current_exe()
        .ok()?
        .ancestors()
        .nth(3)
        .map(Path::to_path_buf)
}

// --- plugin config --------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct Palette {
    tokens: Tokens,
    pane_bg: Option<String>,
    window_bg: Option<String>,
    light: Option<Box<Palette>>,
}

impl Palette {
    fn effective(&self, light: bool) -> Tokens {
        let mut t = self.tokens.clone();
        if light {
            if let Some(l) = &self.light {
                t.extend(l.tokens.clone());
            }
        }
        t
    }
    fn accent(&self, light: bool) -> Option<String> {
        self.effective(light).get("accent").cloned()
    }
    fn pane_bg(&self, light: bool) -> Option<String> {
        if light {
            if let Some(l) = &self.light {
                if let Some(bg) = l
                    .pane_bg
                    .clone()
                    .or_else(|| l.tokens.get("sidebar_bg").cloned())
                {
                    return Some(bg);
                }
            }
        }
        self.pane_bg
            .clone()
            .or_else(|| self.tokens.get("sidebar_bg").cloned())
    }
    /// Outer terminal window colour; falls back to the pane tint so one
    /// palette colours the whole window without extra configuration.
    fn window_bg(&self, light: bool) -> Option<String> {
        if light {
            if let Some(bg) = self.light.as_ref().and_then(|l| l.window_bg.clone()) {
                return Some(bg);
            }
        }
        self.window_bg.clone().or_else(|| self.pane_bg(light))
    }
}

#[derive(Debug)]
struct Rule {
    label: Option<String>,
    path: Option<String>,
    palette: String,
}

#[derive(Debug)]
struct AgentRule {
    session: String,
    palette: String,
}

#[derive(Debug)]
struct PluginConfig {
    auto: bool,
    herdr_config: Option<String>,
    sidebar: bool,
    marker: String,
    pane_tint: bool,
    window_tint: bool,
    palettes: BTreeMap<String, Palette>,
    rules: Vec<Rule>,
    agents: Vec<AgentRule>,
}

fn plugin_config_path(ctx: &Ctx) -> PathBuf {
    ctx.config_dir.join("config.toml")
}

fn load_plugin_config(ctx: &Ctx) -> Res<PluginConfig> {
    let path = plugin_config_path(ctx);
    if !path.exists() {
        fs::create_dir_all(&ctx.config_dir)
            .map_err(|e| format!("create {}: {e}", ctx.config_dir.display()))?;
        fs::write(&path, DEFAULT_CONFIG).map_err(|e| format!("seed {}: {e}", path.display()))?;
        eprintln!("[{TAG}] wrote default config to {}", path.display());
    }
    let text = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    parse_plugin_config(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn parse_palette_tokens(
    name: &str,
    entries: &dyn toml_edit::TableLike,
    allow_light: bool,
) -> Res<Palette> {
    let mut p = Palette::default();
    for (key, v) in entries.iter() {
        match key {
            "light" if allow_light => {
                let sub = v
                    .as_table_like()
                    .ok_or_else(|| format!("palettes.{name}.light must be a table"))?;
                p.light = Some(Box::new(parse_palette_tokens(
                    &format!("{name}.light"),
                    sub,
                    false,
                )?));
            }
            "pane_bg" | "window_bg" => {
                let c = v
                    .as_str()
                    .ok_or_else(|| format!("palettes.{name}.{key} must be a string"))?;
                if !is_hex_colour(c) {
                    return Err(format!(
                        "palettes.{name}.{key} = {c:?}: colours must be #rgb or #rrggbb"
                    ));
                }
                if key == "pane_bg" {
                    p.pane_bg = Some(c.to_owned());
                } else {
                    p.window_bg = Some(c.to_owned());
                }
            }
            token => {
                let c = v
                    .as_str()
                    .ok_or_else(|| format!("palettes.{name}.{token} must be a string"))?;
                if !THEME_TOKENS.contains(&token) {
                    return Err(format!(
                        "palettes.{name}.{token}: not a theme.custom token (allowed: {}, plus pane_bg, window_bg and a light sub-table)",
                        THEME_TOKENS.join(", ")
                    ));
                }
                if !is_hex_colour(c) {
                    return Err(format!(
                        "palettes.{name}.{token} = {c:?}: colours must be #rgb or #rrggbb"
                    ));
                }
                p.tokens.insert(token.to_owned(), c.to_owned());
            }
        }
    }
    Ok(p)
}

fn parse_plugin_config(text: &str) -> Res<PluginConfig> {
    let doc: DocumentMut = text.parse().map_err(|e| format!("parse error: {e}"))?;

    let auto = doc.get("auto").and_then(Item::as_bool).unwrap_or(true);
    let herdr_config = doc
        .get("herdr_config")
        .and_then(Item::as_str)
        .map(str::to_owned);
    let sidebar = doc
        .get("sidebar")
        .and_then(Item::as_table_like)
        .and_then(|t| t.get("enabled"))
        .and_then(Item::as_bool)
        .unwrap_or(true);
    let marker = doc
        .get("sidebar")
        .and_then(Item::as_table_like)
        .and_then(|t| t.get("marker"))
        .and_then(Item::as_str)
        .unwrap_or("●")
        .to_owned();
    let pane_tint = doc
        .get("pane")
        .and_then(Item::as_table_like)
        .and_then(|t| t.get("tint"))
        .and_then(Item::as_bool)
        .unwrap_or(true);
    let window_tint = doc
        .get("window")
        .and_then(Item::as_table_like)
        .and_then(|t| t.get("tint"))
        .and_then(Item::as_bool)
        .unwrap_or(true);

    let mut palettes = BTreeMap::new();
    if let Some(table) = doc.get("palettes").and_then(Item::as_table_like) {
        for (name, item) in table.iter() {
            let entries = item
                .as_table_like()
                .ok_or_else(|| format!("palettes.{name} must be a table of token = \"#hex\""))?;
            palettes.insert(name.to_owned(), parse_palette_tokens(name, entries, true)?);
        }
    }

    let mut rules = Vec::new();
    if let Some(list) = doc.get("workspaces").and_then(Item::as_array_of_tables) {
        for (i, t) in list.iter().enumerate() {
            let label = t.get("label").and_then(Item::as_str).map(str::to_owned);
            let path = t.get("path").and_then(Item::as_str).map(str::to_owned);
            if label.is_none() && path.is_none() {
                return Err(format!("workspaces[{i}] needs `label` or `path`"));
            }
            let palette = t
                .get("palette")
                .and_then(Item::as_str)
                .ok_or_else(|| format!("workspaces[{i}] needs `palette`"))?;
            rules.push(Rule {
                label,
                path,
                palette: palette.to_owned(),
            });
        }
    }

    let mut agents = Vec::new();
    if let Some(list) = doc.get("agents").and_then(Item::as_array_of_tables) {
        for (i, t) in list.iter().enumerate() {
            let session = t
                .get("session")
                .and_then(Item::as_str)
                .ok_or_else(|| format!("agents[{i}] needs `session`"))?;
            let palette = t
                .get("palette")
                .and_then(Item::as_str)
                .ok_or_else(|| format!("agents[{i}] needs `palette`"))?;
            agents.push(AgentRule {
                session: session.to_owned(),
                palette: palette.to_owned(),
            });
        }
    }

    let cfg = PluginConfig {
        auto,
        herdr_config,
        sidebar,
        marker,
        pane_tint,
        window_tint,
        palettes,
        rules,
        agents,
    };
    validate_plugin_config(&cfg)?;
    Ok(cfg)
}

fn validate_plugin_config(cfg: &PluginConfig) -> Res<()> {
    if cfg.palettes.is_empty() {
        return Err("no palettes defined".into());
    }
    if cfg.palettes.len() > 14 && cfg.sidebar {
        return Err("sidebar rows allow at most 16 tokens per row; keep to 14 palettes or set sidebar.enabled = false".into());
    }
    for (name, palette) in &cfg.palettes {
        if palette.tokens.is_empty() {
            return Err(format!("palettes.{name} is empty"));
        }
        if cfg.sidebar && !palette.tokens.contains_key("accent") {
            return Err(format!(
                "palettes.{name} needs `accent` (it colours the sidebar marker)"
            ));
        }
    }
    if cfg.marker.trim().is_empty() || cfg.marker.chars().count() > 8 {
        return Err("sidebar.marker must be 1–8 characters".into());
    }
    for rule in &cfg.rules {
        if rule.label.as_deref().is_none_or(|s| s.trim().is_empty())
            && rule.path.as_deref().is_none_or(|s| s.trim().is_empty())
        {
            return Err("workspaces[] needs a non-empty `label` or `path`".into());
        }
        if !cfg.palettes.contains_key(&rule.palette) {
            return Err(format!(
                "workspaces[].palette = {:?} is not a defined palette",
                rule.palette
            ));
        }
    }
    for a in &cfg.agents {
        if a.session.trim().is_empty() {
            return Err("agents[].session must not be empty".into());
        }
        if !cfg.palettes.contains_key(&a.palette) {
            return Err(format!(
                "agents[].palette = {:?} is not a defined palette",
                a.palette
            ));
        }
    }
    Ok(())
}

fn is_hex_colour(s: &str) -> bool {
    let Some(hex) = s.strip_prefix('#') else {
        return false;
    };
    matches!(hex.len(), 3 | 6) && hex.chars().all(|c| c.is_ascii_hexdigit())
}

// --- state ----------------------------------------------------------------------

/// State tied to the shared herdr `config.toml` and to terminal windows. One
/// copy for every session, guarded by `apply.lock`.
#[derive(Default)]
struct State {
    managed: Vec<String>,
    /// Sidebar kind → canonical form of the rows last written there. An empty
    /// fingerprint comes from a version that did not record one.
    managed_sidebar: BTreeMap<String, String>,
    window: window::WindowState,
}

/// State keyed by herdr ids. Each session runs its own server with its own id
/// space (`w1`, `w1:p1` exist in every session) while plugin state is shared,
/// so these live in one file per session, guarded by that session's lock.
#[derive(Default, Debug, PartialEq)]
struct SessionState {
    pane_tokens: BTreeMap<String, String>,
    ws_tokens: BTreeMap<String, String>,
    last_workspace: Option<String>,
    last_palette: Option<String>,
    /// Identity of the server the caches were written against.
    server: Option<String>,
}

impl SessionState {
    /// Reported tokens live in the server's memory only: a restarted server
    /// restores the same ids without their tokens, so caches written against
    /// another server instance are void.
    fn adopt_server(&mut self, server: Option<&str>) {
        if self.server.as_deref() != server {
            self.ws_tokens.clear();
            self.pane_tokens.clear();
            self.server = server.map(str::to_owned);
        }
    }
}

/// The herdr server a command talks to.
struct Session {
    name: String,
    socket: Option<String>,
    key: String,
    /// The socket file's inode and mtime; a server start creates a new socket.
    server: Option<String>,
}

fn server_stamp(socket: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt as _;
    let m = fs::metadata(socket).ok()?;
    Some(format!("{}:{}:{}", m.ino(), m.mtime(), m.mtime_nsec()))
}

/// File-name-safe key for a session. The socket path identifies the server;
/// the name is the fallback when no socket is known.
fn session_key(socket: Option<&str>, name: &str) -> String {
    let seed = match socket {
        Some(s) => format!("socket:{s}"),
        None => format!("session:{name}"),
    };
    format!("{:016x}", fnv1a(&seed))
}

/// herdr sets `HERDR_SOCKET_PATH` for plugin commands. From a plain shell the
/// socket is looked up by session name, so the CLI and the hooks agree on the
/// key.
fn current_session(ctx: &Ctx) -> Session {
    let name = window::current_session();
    let socket = env::var("HERDR_SOCKET_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| lookup_socket(ctx, &name));
    Session {
        key: session_key(socket.as_deref(), &name),
        server: socket.as_deref().and_then(|s| server_stamp(Path::new(s))),
        name,
        socket,
    }
}

fn lookup_socket(ctx: &Ctx, name: &str) -> Option<String> {
    let v = herdr_json(ctx, &["session", "list", "--json"]).ok()?;
    v["sessions"]
        .as_array()?
        .iter()
        .find(|s| s["name"].as_str() == Some(name))
        .and_then(|s| s["socket_path"].as_str())
        .map(str::to_owned)
}

fn state_path(ctx: &Ctx) -> PathBuf {
    ctx.state_dir.join("state.json")
}

fn session_state_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("state-{key}.json"))
}

fn session_lock_name(key: &str) -> String {
    format!("apply-{key}.lock")
}

fn str_vec(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn str_map(v: &Value) -> BTreeMap<String, String> {
    v.as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Up to 0.3.3 `state.json` also held the id caches of whichever session wrote
/// last. Those keys are ignored here and dropped on the next save; the cost is
/// one redundant report per pane and Space on the first sweep.
fn load_state(ctx: &Ctx) -> State {
    let Some(v) = read_json(&state_path(ctx)) else {
        return State::default();
    };
    State {
        managed: str_vec(&v["managed"]),
        managed_sidebar: match v["sidebar_rows"].as_object() {
            Some(_) => str_map(&v["sidebar_rows"]),
            None => str_vec(&v["managed_sidebar"])
                .into_iter()
                .map(|k| (k, String::new()))
                .collect(),
        },
        window: window::WindowState {
            original: str_map(&v["window_original"]),
            current: str_map(&v["window_current"]),
            session: str_map(&v["window_session"]),
        },
    }
}

fn write_state_file(dir: &Path, path: &Path, v: &Value) -> Res<()> {
    fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    fs::write(path, serde_json::to_string_pretty(v).unwrap())
        .map_err(|e| format!("write {}: {e}", path.display()))
}

fn save_state(ctx: &Ctx, st: &State) -> Res<()> {
    let v = serde_json::json!({
        "managed": st.managed,
        "sidebar_rows": st.managed_sidebar,
        "managed_sidebar": st.managed_sidebar.keys().collect::<Vec<_>>(),
        "window_original": st.window.original,
        "window_current": st.window.current,
        "window_session": st.window.session,
    });
    write_state_file(&ctx.state_dir, &state_path(ctx), &v)
}

fn read_session_state(dir: &Path, key: &str) -> SessionState {
    let Some(v) = read_json(&session_state_path(dir, key)) else {
        return SessionState::default();
    };
    SessionState {
        pane_tokens: str_map(&v["pane_tokens"]),
        ws_tokens: str_map(&v["ws_tokens"]),
        last_workspace: v["last_workspace"].as_str().map(str::to_owned),
        last_palette: v["last_palette"].as_str().map(str::to_owned),
        server: v["server"].as_str().map(str::to_owned),
    }
}

fn load_session_state(dir: &Path, session: &Session) -> SessionState {
    let mut st = read_session_state(dir, &session.key);
    st.adopt_server(session.server.as_deref());
    st
}

fn save_session_state(dir: &Path, session: &Session, st: &SessionState) -> Res<()> {
    let v = serde_json::json!({
        "session": session.name,
        "socket": session.socket,
        "pane_tokens": st.pane_tokens,
        "ws_tokens": st.ws_tokens,
        "last_workspace": st.last_workspace,
        "last_palette": st.last_palette,
        "server": st.server,
    });
    write_state_file(dir, &session_state_path(dir, &session.key), &v)
}

/// One writer at a time. `workspace.focused` can fire in bursts, and several
/// herdr sessions start (and run their startup `apply`) at the same moment, so
/// a second invocation waits for the first instead of racing it.
struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn acquire_lock(dir: &Path, name: &str) -> Res<Option<Lock>> {
    fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    acquire_lock_at(&dir.join(name), LOCK_WAIT)
}

/// Take the lock file at `path`, retrying with backoff for up to `wait`.
/// `None` means another holder kept it for the whole wait. A lock older than
/// `LOCK_STALE` belongs to a process that died and is taken over.
fn acquire_lock_at(path: &Path, wait: Duration) -> Res<Option<Lock>> {
    let deadline = Instant::now() + wait;
    let mut backoff = Duration::from_millis(20);
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(_) => return Ok(Some(Lock(path.to_owned()))),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(path)
                    .and_then(|m| m.modified())
                    .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() > LOCK_STALE)
                    .unwrap_or(false);
                if stale {
                    let _ = fs::remove_file(path);
                    continue;
                }
            }
            Err(e) => return Err(format!("lock {}: {e}", path.display())),
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        std::thread::sleep(backoff.min(deadline - now));
        backoff = (backoff * 2).min(Duration::from_millis(250));
    }
}

// --- herdr ----------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Workspace {
    id: String,
    label: String,
    cwd: String,
    focused: bool,
}

#[derive(Clone, Debug)]
struct Pane {
    id: String,
    workspace_id: String,
    agent: Option<String>,
    session: Option<String>,
}

fn herdr_json(ctx: &Ctx, args: &[&str]) -> Res<Value> {
    let out = Command::new(&ctx.herdr)
        .args(args)
        .output()
        .map_err(|e| format!("run {} {}: {e}", ctx.herdr, args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "`herdr {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let v: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("`herdr {}`: bad JSON: {e}", args.join(" ")))?;
    Ok(v.get("result").cloned().unwrap_or(v))
}

fn herdr_run(ctx: &Ctx, args: &[&str]) -> Res<()> {
    let out = Command::new(&ctx.herdr)
        .args(args)
        .output()
        .map_err(|e| format!("run {} {}: {e}", ctx.herdr, args.join(" ")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "`herdr {}` failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Failures in hooks are otherwise invisible; surface them as a toast.
fn notify(ctx: &Ctx, body: &str) {
    let _ = Command::new(&ctx.herdr)
        .args([
            "notification",
            "show",
            "Space Colors",
            "--body",
            body,
            "--sound",
            "none",
        ])
        .output();
}

/// Event hooks get `data.workspace_id`; actions get `HERDR_WORKSPACE_ID`; plain
/// CLI use falls back to whatever is focused.
fn resolve_workspace_id(ctx: &Ctx) -> Res<String> {
    if let Ok(raw) = env::var("HERDR_PLUGIN_EVENT_JSON") {
        if let Ok(ev) = serde_json::from_str::<Value>(&raw) {
            let data = ev.get("data").unwrap_or(&ev);
            for candidate in [
                data.get("workspace_id"),
                data.get("workspace").and_then(|w| w.get("workspace_id")),
                ev.get("workspace_id"),
            ] {
                if let Some(id) = candidate.and_then(Value::as_str) {
                    return Ok(id.to_owned());
                }
            }
        }
    }
    if let Ok(id) = env::var("HERDR_WORKSPACE_ID") {
        if !id.is_empty() {
            return Ok(id);
        }
    }
    all_workspaces(ctx)?
        .into_iter()
        .find(|w| w.focused)
        .map(|w| w.id)
        .ok_or_else(|| "no focused workspace".into())
}

fn all_workspaces(ctx: &Ctx) -> Res<Vec<Workspace>> {
    let list = herdr_json(ctx, &["workspace", "list"])?;
    let cwds = pane_cwds(ctx)?;
    let mut out = Vec::new();
    for w in list["workspaces"].as_array().into_iter().flatten() {
        let id = w["workspace_id"].as_str().unwrap_or_default().to_owned();
        if id.is_empty() {
            continue;
        }
        out.push(Workspace {
            cwd: cwds.get(&id).cloned().unwrap_or_default(),
            label: w["label"].as_str().unwrap_or_default().to_owned(),
            focused: w["focused"].as_bool().unwrap_or(false),
            id,
        });
    }
    Ok(out)
}

/// `workspace get` carries no cwd; the panes do. The first pane's cwd is the
/// workspace's project directory for every layout the manager plugin builds.
fn pane_cwds(ctx: &Ctx) -> Res<HashMap<String, String>> {
    let panes = herdr_json(ctx, &["pane", "list"])?;
    let mut m = HashMap::new();
    for p in panes["panes"].as_array().into_iter().flatten() {
        let ws = p["workspace_id"].as_str().unwrap_or_default();
        let cwd = p["cwd"].as_str().unwrap_or_default();
        if !ws.is_empty() && !cwd.is_empty() {
            m.entry(ws.to_owned()).or_insert_with(|| cwd.to_owned());
        }
    }
    Ok(m)
}

fn all_panes(ctx: &Ctx) -> Res<Vec<Pane>> {
    let panes = herdr_json(ctx, &["pane", "list"])?;
    let agents = herdr_json(ctx, &["agent", "list"])?;
    let mut by_pane: HashMap<String, (String, Option<String>)> = HashMap::new();
    for a in agents["agents"].as_array().into_iter().flatten() {
        let pid = a["pane_id"].as_str().unwrap_or_default().to_owned();
        let kind = a["agent"].as_str().unwrap_or_default().to_owned();
        let session = a["agent_session"]["value"].as_str().map(str::to_owned);
        by_pane.insert(pid, (kind, session));
    }
    let mut out = Vec::new();
    for p in panes["panes"].as_array().into_iter().flatten() {
        let id = p["pane_id"].as_str().unwrap_or_default().to_owned();
        if id.is_empty() {
            continue;
        }
        let (agent, session) = by_pane
            .remove(&id)
            .map(|(k, s)| (Some(k), s))
            .unwrap_or((None, None));
        out.push(Pane {
            workspace_id: p["workspace_id"].as_str().unwrap_or_default().to_owned(),
            agent,
            session,
            id,
        });
    }
    Ok(out)
}

fn find_workspace(ctx: &Ctx, key: &str) -> Res<Workspace> {
    let all = all_workspaces(ctx)?;
    all.iter()
        .find(|w| w.id == key)
        .or_else(|| all.iter().find(|w| w.label == key))
        .cloned()
        .ok_or_else(|| format!("no workspace with id or label {key:?}"))
}

// --- palette selection ----------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Via {
    Agent,
    Rule,
    Auto,
    None,
}

fn choose_palette<'a>(cfg: &'a PluginConfig, ws: &Workspace) -> (Option<&'a str>, Via) {
    let mut best: Option<(usize, &Rule)> = None;
    for rule in &cfg.rules {
        let score = if rule.label.as_deref() == Some(ws.label.as_str()) {
            Some(usize::MAX)
        } else if let Some(p) = &rule.path {
            let m = expand_home(p);
            let m = m.to_string_lossy();
            let m = m.trim_end_matches('/');
            (!ws.cwd.is_empty() && (ws.cwd == m || ws.cwd.starts_with(&format!("{m}/"))))
                .then_some(m.len())
        } else {
            None
        };
        if let Some(s) = score {
            if best.is_none_or(|(b, _)| s > b) {
                best = Some((s, rule));
            }
        }
    }
    if let Some((_, rule)) = best {
        return (Some(&rule.palette), Via::Rule);
    }
    if !cfg.auto {
        return (None, Via::None);
    }
    let names: Vec<&str> = cfg.palettes.keys().map(String::as_str).collect();
    let key = if ws.cwd.is_empty() {
        &ws.label
    } else {
        &ws.cwd
    };
    (
        Some(names[(fnv1a(key) % names.len() as u64) as usize]),
        Via::Auto,
    )
}

/// An agent rule matches the persisted session id exactly, or a pi session
/// path by its file name / id suffix, so both forms of `herdr agent list`
/// output can be pinned.
fn agent_palette<'a>(cfg: &'a PluginConfig, session: Option<&str>) -> Option<&'a str> {
    let s = session?;
    let file = Path::new(s);
    let base = file.file_name().and_then(|f| f.to_str()).unwrap_or(s);
    let stem = file.file_stem().and_then(|f| f.to_str()).unwrap_or(base);
    cfg.agents
        .iter()
        .find(|a| {
            a.session == s
                || a.session == base
                || a.session == stem
                || stem.ends_with(&a.session)
                || s.ends_with(&a.session)
        })
        .map(|a| a.palette.as_str())
}

fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

// --- herdr config editing (pure) ------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum ThemeMode {
    Dark,
    Light,
    Auto,
}

fn theme_mode(doc: &DocumentMut) -> ThemeMode {
    let theme = doc.get("theme").and_then(Item::as_table_like);
    if theme
        .and_then(|t| t.get("auto_switch"))
        .and_then(Item::as_bool)
        .unwrap_or(false)
    {
        return ThemeMode::Auto;
    }
    let name = theme
        .and_then(|t| t.get("name"))
        .and_then(Item::as_str)
        .unwrap_or("catppuccin")
        .to_lowercase();
    if LIGHT_THEME_HINTS.iter().any(|h| name.contains(h)) {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    }
}

/// Walk/create standard tables along `path`; intermediate ones stay implicit
/// so no bare `[ui.sidebar]` header is emitted. Index auto-vivification is
/// avoided on purpose: it creates inline tables.
fn ensure_table<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> Res<&'a mut Table> {
    let mut cur: &mut Table = doc.as_table_mut();
    for (i, key) in path.iter().enumerate() {
        if cur.get(key).is_none() {
            let mut t = Table::new();
            t.set_implicit(i + 1 < path.len());
            cur.insert(key, Item::Table(t));
        }
        cur = cur
            .get_mut(key)
            .and_then(Item::as_table_mut)
            .ok_or_else(|| {
                format!(
                    "{} exists but is not a standard table",
                    path[..=i].join(".")
                )
            })?;
    }
    Ok(cur)
}

fn apply_theme(doc: &mut DocumentMut, tokens: &Tokens, previously_managed: &[String]) -> Res<()> {
    let table: &mut dyn toml_edit::TableLike = {
        if doc.get("theme").is_none() {
            let mut t = Table::new();
            t.set_implicit(true);
            doc.insert("theme", Item::Table(t));
        }
        let theme = doc
            .get_mut("theme")
            .and_then(Item::as_table_like_mut)
            .ok_or("theme exists but is not a table")?;
        if theme.get("custom").is_none() {
            theme.insert("custom", Item::Table(Table::new()));
        }
        theme
            .get_mut("custom")
            .and_then(Item::as_table_like_mut)
            .ok_or("theme.custom exists but is not a table")?
    };
    for key in previously_managed {
        if !tokens.contains_key(key) {
            table.remove(key);
        }
    }
    for (token, colour) in tokens {
        table.insert(token, value(colour));
    }
    Ok(())
}

fn clear_theme(doc: &mut DocumentMut, managed: &[String]) {
    if let Some(table) = doc
        .get_mut("theme")
        .and_then(Item::as_table_like_mut)
        .and_then(|t| t.get_mut("custom"))
        .and_then(Item::as_table_like_mut)
    {
        for key in managed {
            table.remove(key);
        }
    }
}

/// Drop `[theme.custom]` / `[theme]` when the plugin emptied them (standard or
/// inline), and keep `[theme]` implicit when it only holds sub-tables.
fn tidy_theme(doc: &mut DocumentMut) {
    let theme_empty = {
        let Some(theme) = doc.get_mut("theme") else {
            return;
        };
        if let Some(t) = theme.as_table_like_mut() {
            let custom_empty = t
                .get("custom")
                .and_then(Item::as_table_like)
                .is_some_and(|c| c.is_empty());
            if custom_empty {
                t.remove("custom");
            }
        }
        theme.as_table_like().is_some_and(|t| t.is_empty())
    };
    if theme_empty {
        doc.remove("theme");
        return;
    }
    if let Some(t) = doc.get_mut("theme").and_then(Item::as_table_mut) {
        if t.iter().all(|(_, v)| v.is_table()) {
            t.set_implicit(true);
        }
    }
}

struct SidebarSpec {
    /// (palette name, fg colour) — one token per palette.
    colours: Vec<(String, String)>,
}

const SIDEBAR_KINDS: [&str; 2] = ["agents", "spaces"];

fn build_rows(spec: &SidebarSpec, kind: &str) -> Array {
    let mut first = Array::new();
    first.push("state_icon");
    for (name, fg) in &spec.colours {
        let mut it = InlineTable::new();
        it.insert("token", format!("${TOKEN_PREFIX}{name}").into());
        it.insert("fg", fg.as_str().into());
        it.insert("bold", true.into());
        first.push(TomlValue::InlineTable(it));
    }
    first.push("workspace");
    if kind == "agents" {
        first.push("tab");
    }
    let mut second = Array::new();
    if kind == "agents" {
        second.push("agent");
    } else {
        second.push("branch");
        second.push("git_status");
    }
    let mut rows = Array::new();
    rows.push(TomlValue::Array(first));
    rows.push(TomlValue::Array(second));
    rows
}

/// Write managed sidebar rows for each kind the user has not customised.
/// Returns the kinds now managed.
fn apply_sidebar(
    doc: &mut DocumentMut,
    spec: &SidebarSpec,
    already_managed: &[String],
) -> Res<Vec<String>> {
    let mut managed = Vec::new();
    for kind in SIDEBAR_KINDS {
        let user_defined = doc
            .get("ui")
            .and_then(Item::as_table_like)
            .and_then(|u| u.get("sidebar"))
            .and_then(Item::as_table_like)
            .and_then(|s| s.get(kind))
            .and_then(Item::as_table_like)
            .is_some_and(|k| k.get("rows").is_some())
            && !already_managed.iter().any(|m| m == kind);
        if user_defined {
            continue;
        }
        let table = ensure_table(doc, &["ui", "sidebar", kind])?;
        table.insert(
            "rows",
            Item::Value(TomlValue::Array(build_rows(spec, kind))),
        );
        managed.push(kind.to_owned());
    }
    Ok(managed)
}

/// Formatting-independent form of a TOML value, for comparing rows.
fn canon(v: &TomlValue) -> String {
    match v {
        TomlValue::String(s) => format!("{:?}", s.value()),
        TomlValue::Array(a) => format!("[{}]", a.iter().map(canon).collect::<Vec<_>>().join(",")),
        TomlValue::InlineTable(t) => {
            let mut kv: Vec<String> = t.iter().map(|(k, v)| format!("{k}={}", canon(v))).collect();
            kv.sort();
            format!("{{{}}}", kv.join(","))
        }
        other => other.clone().decorated("", "").to_string(),
    }
}

fn rows_canon(doc: &DocumentMut, kind: &str) -> Option<String> {
    let item = doc
        .get("ui")
        .and_then(Item::as_table_like)
        .and_then(|u| u.get("sidebar"))
        .and_then(Item::as_table_like)
        .and_then(|s| s.get(kind))
        .and_then(Item::as_table_like)
        .and_then(|k| k.get("rows"))?;
    Some(
        item.as_value()
            .map(canon)
            .unwrap_or_else(|| item.to_string()),
    )
}

/// Splits the managed kinds into those whose rows are still exactly what this
/// plugin wrote (or absent) and those someone else has replaced since. Rows
/// recorded without a fingerprint count as ours only if they match what the
/// plugin generates for one of `legacy`.
fn owned_sidebar(
    doc: &DocumentMut,
    managed: &BTreeMap<String, String>,
    legacy: &[SidebarSpec],
) -> (Vec<String>, Vec<String>) {
    managed.keys().cloned().partition(|kind| {
        let Some(current) = rows_canon(doc, kind) else {
            return true;
        };
        match managed[kind].as_str() {
            "" => legacy
                .iter()
                .any(|spec| canon(&TomlValue::Array(build_rows(spec, kind))) == current),
            written => written == current,
        }
    })
}

fn clear_sidebar(doc: &mut DocumentMut, managed: &[String]) {
    for kind in managed {
        let Some(sidebar) = doc
            .get_mut("ui")
            .and_then(Item::as_table_like_mut)
            .and_then(|u| u.get_mut("sidebar"))
            .and_then(Item::as_table_like_mut)
        else {
            continue;
        };
        if let Some(k) = sidebar.get_mut(kind).and_then(Item::as_table_like_mut) {
            k.remove("rows");
        }
        if sidebar
            .get(kind)
            .and_then(Item::as_table_like)
            .is_some_and(|k| k.is_empty())
        {
            sidebar.remove(kind);
        }
    }
    let sidebar_empty = doc
        .get("ui")
        .and_then(Item::as_table_like)
        .and_then(|u| u.get("sidebar"))
        .and_then(Item::as_table_like)
        .is_some_and(|s| s.is_empty());
    if sidebar_empty {
        if let Some(u) = doc.get_mut("ui").and_then(Item::as_table_like_mut) {
            u.remove("sidebar");
        }
    }
    if doc
        .get("ui")
        .and_then(Item::as_table_like)
        .is_some_and(|u| u.is_empty())
    {
        doc.remove("ui");
    }
}

struct Plan<'a> {
    theme: Option<&'a Tokens>,
    previously_managed: &'a [String],
    sidebar: Option<&'a SidebarSpec>,
    managed_sidebar: &'a BTreeMap<String, String>,
    legacy: &'a [SidebarSpec],
}

struct Planned {
    text: String,
    managed_sidebar: BTreeMap<String, String>,
    /// Kinds whose rows were changed by someone else and are no longer ours.
    released: Vec<String>,
}

/// Full apply: theme tokens (or their removal when `theme` is None) plus
/// sidebar rows. Rows replaced by someone else since the last write are left
/// alone and released.
fn plan_full(original: &str, plan: &Plan) -> Res<Planned> {
    let mut doc: DocumentMut = original
        .parse()
        .map_err(|e| format!("herdr config parse error: {e}"))?;
    match plan.theme {
        Some(tokens) => apply_theme(&mut doc, tokens, plan.previously_managed)?,
        None => clear_theme(&mut doc, plan.previously_managed),
    }
    let (owned, released) = owned_sidebar(&doc, plan.managed_sidebar, plan.legacy);
    let managed_sidebar = match plan.sidebar {
        Some(spec) => apply_sidebar(&mut doc, spec, &owned)?
            .into_iter()
            .map(|kind| {
                let fp = rows_canon(&doc, &kind).unwrap_or_default();
                (kind, fp)
            })
            .collect(),
        None => {
            clear_sidebar(&mut doc, &owned);
            BTreeMap::new()
        }
    };
    tidy_theme(&mut doc);
    Ok(Planned {
        text: doc.to_string(),
        managed_sidebar,
        released,
    })
}

fn report_released(released: &[String]) {
    for kind in released {
        outln!("[{TAG}] sidebar {kind} rows were changed by someone else; leaving them alone");
    }
}

#[cfg(test)]
fn plan_apply(original: &str, tokens: &Tokens, previously_managed: &[String]) -> Res<String> {
    let plan = Plan {
        theme: Some(tokens),
        previously_managed,
        sidebar: None,
        managed_sidebar: &BTreeMap::new(),
        legacy: &[],
    };
    plan_full(original, &plan).map(|p| p.text)
}

fn plan_clear(
    original: &str,
    managed: &[String],
    managed_sidebar: &BTreeMap<String, String>,
    legacy: &[SidebarSpec],
) -> Res<Planned> {
    let plan = Plan {
        theme: None,
        previously_managed: managed,
        sidebar: None,
        managed_sidebar,
        legacy,
    };
    plan_full(original, &plan)
}

// --- commit ---------------------------------------------------------------------

fn commit(
    ctx: &Ctx,
    path: &Path,
    original: &str,
    updated: &str,
    dry_run: bool,
    what: &str,
) -> Res<bool> {
    if updated == original {
        outln!("[{TAG}] {what}: already in place, nothing to do");
        return Ok(false);
    }
    if dry_run {
        outln!("[{TAG}] {what}: dry run, would change {}:", path.display());
        out!("{}", diff(original, updated));
        return Ok(false);
    }

    ensure_backup(path, original)?;
    write_atomic(path, updated)?;

    if let Err(e) = herdr_run(ctx, &["config", "check"]) {
        write_atomic(path, original)?;
        notify(
            ctx,
            "config check failed after an edit; the previous config.toml was restored",
        );
        return Err(format!(
            "{what}: `herdr config check` rejected the result, restored the previous config.\n{e}"
        ));
    }
    if let Err(e) = herdr_run(ctx, &["server", "reload-config"]) {
        notify(ctx, "config.toml updated but the server reload failed");
        return Err(format!(
            "{what}: config written and valid, but the reload failed (is the server running?)\n{e}"
        ));
    }
    outln!("[{TAG}] {what}: applied");
    Ok(true)
}

fn ensure_backup(path: &Path, original: &str) -> Res<()> {
    let backup = PathBuf::from(format!("{}{BACKUP_SUFFIX}", path.display()));
    if backup.exists() {
        return Ok(());
    }
    fs::write(&backup, original).map_err(|e| format!("write backup {}: {e}", backup.display()))?;
    eprintln!(
        "[{TAG}] first edit: backed up the original to {}",
        backup.display()
    );
    Ok(())
}

fn write_atomic(path: &Path, text: &str) -> Res<()> {
    let tmp = PathBuf::from(format!("{}.{TAG}.tmp", path.display()));
    fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("rename into {}: {e}", path.display()))
}

fn diff(old: &str, new: &str) -> String {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let mut s = String::new();
    for l in &old {
        if !new.contains(l) {
            s.push_str(&format!("- {l}\n"));
        }
    }
    for l in &new {
        if !old.contains(l) {
            s.push_str(&format!("+ {l}\n"));
        }
    }
    s
}

// --- sidebar tagging (sweep) ------------------------------------------------------

struct Assignment {
    ws_palette: BTreeMap<String, (Option<String>, Via)>,
    pane_palette: BTreeMap<String, (Option<String>, Via)>,
}

fn assign(cfg: &PluginConfig, workspaces: &[Workspace], panes: &[Pane]) -> Assignment {
    let mut ws_palette = BTreeMap::new();
    for w in workspaces {
        let (p, via) = choose_palette(cfg, w);
        ws_palette.insert(w.id.clone(), (p.map(str::to_owned), via));
    }
    let mut pane_palette = BTreeMap::new();
    for p in panes {
        let entry = match agent_palette(cfg, p.session.as_deref()) {
            Some(a) => (Some(a.to_owned()), Via::Agent),
            None => ws_palette
                .get(&p.workspace_id)
                .cloned()
                .unwrap_or((None, Via::None)),
        };
        pane_palette.insert(p.id.clone(), entry);
    }
    Assignment {
        ws_palette,
        pane_palette,
    }
}

fn report_args<'a>(
    kind: &'a str,
    id: &'a str,
    palette: Option<&'a str>,
    marker: &'a str,
    all: &'a [String],
) -> Vec<String> {
    let mut args = vec![
        kind.to_owned(),
        "report-metadata".to_owned(),
        id.to_owned(),
        "--source".to_owned(),
        SOURCE.to_owned(),
    ];
    for name in all {
        if Some(name.as_str()) == palette {
            args.push("--token".into());
            args.push(format!("{TOKEN_PREFIX}{name}={marker}"));
        } else {
            args.push("--clear-token".into());
            args.push(format!("{TOKEN_PREFIX}{name}"));
        }
    }
    args
}

/// Entities whose wanted colour differs from what `cache` says is reported.
/// Cache entries for ids that are no longer live are dropped first. The cache
/// must belong to the session whose ids `want` holds.
fn stale_tags(
    cache: &mut BTreeMap<String, String>,
    want: &BTreeMap<String, (Option<String>, Via)>,
) -> Vec<(String, Option<String>)> {
    cache.retain(|k, _| want.contains_key(k));
    want.iter()
        .filter(|(id, (palette, _))| {
            cache.get(*id).map(String::as_str) != Some(palette.as_deref().unwrap_or_default())
        })
        .map(|(id, (palette, _))| (id.clone(), palette.clone()))
        .collect()
}

/// Report one colour token per pane and workspace, clearing the others. Only
/// entities whose colour changed since the last sweep cost a herdr call.
fn sweep(ctx: &Ctx, cfg: &PluginConfig, st: &mut SessionState, dry_run: bool) -> Res<usize> {
    let workspaces = all_workspaces(ctx)?;
    let panes = all_panes(ctx)?;
    let a = assign(cfg, &workspaces, &panes);
    let all: Vec<String> = cfg.palettes.keys().cloned().collect();
    let mut calls = 0;

    for (kind, cache, want) in [
        ("workspace", &mut st.ws_tokens, &a.ws_palette),
        ("pane", &mut st.pane_tokens, &a.pane_palette),
    ] {
        for (id, palette) in stale_tags(cache, want) {
            let shown = palette.clone().unwrap_or_default();
            if dry_run {
                outln!(
                    "[{TAG}] would tag {kind} {id} → {}",
                    if shown.is_empty() { "none" } else { &shown }
                );
            } else {
                let args = report_args(kind, &id, palette.as_deref(), &cfg.marker, &all);
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                herdr_run(ctx, &refs)?;
                cache.insert(id, shown);
            }
            calls += 1;
        }
    }
    Ok(calls)
}

fn clear_tags(ctx: &Ctx, cfg: &PluginConfig, st: &SessionState) -> Res<()> {
    let all: Vec<String> = cfg.palettes.keys().cloned().collect();
    for id in st.ws_tokens.keys() {
        let args = report_args("workspace", id, None, &cfg.marker, &all);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = herdr_run(ctx, &refs);
    }
    for id in st.pane_tokens.keys() {
        let args = report_args("pane", id, None, &cfg.marker, &all);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = herdr_run(ctx, &refs);
    }
    Ok(())
}

fn sidebar_spec(cfg: &PluginConfig, light: bool) -> Option<SidebarSpec> {
    cfg.sidebar.then(|| sidebar_rows_for(cfg, light))
}

fn sidebar_rows_for(cfg: &PluginConfig, light: bool) -> SidebarSpec {
    let colours = cfg
        .palettes
        .iter()
        .filter_map(|(n, p)| p.accent(light).map(|c| (n.clone(), c)))
        .collect();
    SidebarSpec { colours }
}

/// What this plugin would write in either theme mode: the reference for rows
/// recorded before fingerprints existed.
fn legacy_specs(cfg: &PluginConfig) -> [SidebarSpec; 2] {
    [sidebar_rows_for(cfg, false), sidebar_rows_for(cfg, true)]
}

// --- commands -------------------------------------------------------------------

/// Theme and window tint for `ws`. Caller holds `apply.lock`.
fn apply_shared(
    ctx: &Ctx,
    cfg: &PluginConfig,
    ws: &Workspace,
    palette_name: Option<&str>,
    how: &str,
    dry_run: bool,
) -> Res<()> {
    let mut st = load_state(ctx);

    let path = herdr_config_path(cfg);
    let original =
        fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let doc: DocumentMut = original
        .parse()
        .map_err(|e| format!("herdr config parse error: {e}"))?;
    let light = theme_mode(&doc) == ThemeMode::Light;

    let tokens = palette_name.map(|n| cfg.palettes[n].effective(light));
    let spec = sidebar_spec(cfg, light);
    let plan = Plan {
        theme: tokens.as_ref(),
        previously_managed: &st.managed,
        sidebar: spec.as_ref(),
        managed_sidebar: &st.managed_sidebar,
        legacy: &legacy_specs(cfg),
    };
    let Planned {
        text: updated,
        managed_sidebar,
        released,
    } = plan_full(&original, &plan)?;
    report_released(&released);
    let what = match palette_name {
        Some(n) => format!("{} → {n} ({how})", ws.label),
        None => format!("{} → base theme ({how})", ws.label),
    };
    commit(ctx, &path, &original, &updated, dry_run, &what)?;

    if cfg.window_tint {
        let target = palette_name.and_then(|n| cfg.palettes[n].window_bg(light));
        for line in window::apply(&mut st.window, target.as_deref(), dry_run)? {
            outln!("[{TAG}] {line}");
        }
    }

    if !dry_run {
        st.managed = tokens
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default();
        st.managed_sidebar = managed_sidebar;
        save_state(ctx, &st)?;
    }
    Ok(())
}

const LOCK_BUSY: &str = "another apply kept the lock past the wait";

/// The theme lives in the shared `config.toml`, so its write is serialised
/// across sessions by `apply.lock`; the sidebar tags are per session and only
/// take that session's lock. The two are never held together.
fn cmd_apply(dry_run: bool) -> Res<()> {
    let ctx = ctx();
    let cfg = load_plugin_config(&ctx)?;
    let session = current_session(&ctx);

    let id = resolve_workspace_id(&ctx)?;
    let ws = find_workspace(&ctx, &id)?;
    let (palette_name, via) = choose_palette(&cfg, &ws);
    let how = match via {
        Via::Rule => "explicit rule",
        Via::Auto => "auto",
        _ => "no rule, auto off",
    };

    match acquire_lock(&ctx.state_dir, "apply.lock")? {
        Some(_lock) => apply_shared(&ctx, &cfg, &ws, palette_name, how, dry_run)?,
        None => outln!("[{TAG}] {LOCK_BUSY}; theme skipped"),
    }

    let Some(_lock) = acquire_lock(&ctx.state_dir, &session_lock_name(&session.key))? else {
        outln!("[{TAG}] {LOCK_BUSY}; sidebar tags skipped");
        return Ok(());
    };
    let mut sst = load_session_state(&ctx.state_dir, &session);
    let tagged = sweep(&ctx, &cfg, &mut sst, dry_run)?;
    if tagged > 0 {
        outln!("[{TAG}] sidebar tags: {tagged} updated");
    }
    if !dry_run {
        sst.last_workspace = Some(ws.id);
        sst.last_palette = palette_name.map(str::to_owned);
        save_session_state(&ctx.state_dir, &session, &sst)?;
    }
    Ok(())
}

fn cmd_sweep(dry_run: bool) -> Res<()> {
    let ctx = ctx();
    let cfg = load_plugin_config(&ctx)?;
    let session = current_session(&ctx);
    let Some(_lock) = acquire_lock(&ctx.state_dir, &session_lock_name(&session.key))? else {
        outln!("[{TAG}] {LOCK_BUSY}; skipped");
        return Ok(());
    };
    let mut st = load_session_state(&ctx.state_dir, &session);
    let n = sweep(&ctx, &cfg, &mut st, dry_run)?;
    outln!("[{TAG}] sidebar tags: {n} updated");
    if !dry_run {
        save_session_state(&ctx.state_dir, &session, &st)?;
    }
    Ok(())
}

/// Restores the shared config and every tinted window, and drops the tags of
/// the calling session. Other sessions keep their tags, which render nothing
/// once the managed sidebar rows are gone, and their caches stay accurate.
fn cmd_clear(dry_run: bool) -> Res<()> {
    let ctx = ctx();
    let cfg = load_plugin_config(&ctx)?;
    let session = current_session(&ctx);
    {
        let Some(_lock) = acquire_lock(&ctx.state_dir, "apply.lock")? else {
            return Err(format!("{LOCK_BUSY}; try again"));
        };
        let mut st = load_state(&ctx);
        let path = herdr_config_path(&cfg);
        let original =
            fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let planned = plan_clear(
            &original,
            &st.managed,
            &st.managed_sidebar,
            &legacy_specs(&cfg),
        )?;
        report_released(&planned.released);
        commit(&ctx, &path, &original, &planned.text, dry_run, "clear")?;
        if !dry_run {
            for line in window::restore_all(&mut st.window)? {
                outln!("[{TAG}] {line}");
            }
            save_state(&ctx, &State::default())?;
        }
    }

    let Some(_lock) = acquire_lock(&ctx.state_dir, &session_lock_name(&session.key))? else {
        return Err(format!("{LOCK_BUSY}; try again"));
    };
    let sst = load_session_state(&ctx.state_dir, &session);
    if dry_run {
        outln!(
            "[{TAG}] clear: would drop tags on {} workspaces and {} panes",
            sst.ws_tokens.len(),
            sst.pane_tokens.len()
        );
        return Ok(());
    }
    clear_tags(&ctx, &cfg, &sst)?;
    save_session_state(&ctx.state_dir, &session, &SessionState::default())?;
    Ok(())
}

fn cmd_status() -> Res<()> {
    let ctx = ctx();
    let cfg = load_plugin_config(&ctx)?;
    let st = load_state(&ctx);
    let path = herdr_config_path(&cfg);
    let mode = fs::read_to_string(&path)
        .ok()
        .and_then(|t| t.parse::<DocumentMut>().ok())
        .map(|d| theme_mode(&d));

    outln!("herdr config:  {}", path.display());
    let session = current_session(&ctx);
    outln!("state:         {}", state_path(&ctx).display());
    outln!(
        "session:       {} ({})",
        session.name,
        session.socket.as_deref().unwrap_or("socket unknown")
    );
    outln!(
        "session state: {}",
        session_state_path(&ctx.state_dir, &session.key).display()
    );
    outln!(
        "theme mode:    {}",
        match mode {
            Some(ThemeMode::Dark) => "dark".to_owned(),
            Some(ThemeMode::Light) =>
                "light (using palettes' light variants where defined)".to_owned(),
            Some(ThemeMode::Auto) =>
                "auto_switch — host appearance unknown to plugins; dark palettes used".to_owned(),
            None => "unknown".to_owned(),
        }
    );
    outln!(
        "theme keys:    {}",
        if st.managed.is_empty() {
            "none".into()
        } else {
            st.managed.join(", ")
        }
    );
    outln!(
        "sidebar rows:  {}",
        if !cfg.sidebar {
            "disabled".into()
        } else if st.managed_sidebar.is_empty() {
            "not managed yet (or user-defined)".into()
        } else {
            let kinds: Vec<&str> = st.managed_sidebar.keys().map(String::as_str).collect();
            format!("managed: {}", kinds.join(", "))
        }
    );
    outln!(
        "pane tint:     {}",
        if cfg.pane_tint {
            "on (needs the shell hook; run `shell-hook`)"
        } else {
            "off"
        }
    );
    outln!(
        "window tint:   {}",
        if !cfg.window_tint {
            "off".to_owned()
        } else if st.window.current.is_empty() {
            "on (Apple Terminal; nothing tinted right now)".to_owned()
        } else {
            st.window
                .current
                .iter()
                .map(|(tty, hex)| format!("{tty} {hex}"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    outln!();

    let workspaces = all_workspaces(&ctx)?;
    let panes = all_panes(&ctx)?;
    let a = assign(&cfg, &workspaces, &panes);
    let home_s = home().to_string_lossy().to_string();

    outln!(
        "{:<4} {:<2} {:<18} {:<10} {:<9} CWD",
        "WS",
        "",
        "LABEL",
        "PALETTE",
        "VIA"
    );
    for w in &workspaces {
        let (p, via) = &a.ws_palette[&w.id];
        outln!(
            "{:<4} {:<2} {:<18} {:<10} {:<9} {}",
            w.id,
            if w.focused { "*" } else { "" },
            w.label,
            p.as_deref().unwrap_or("-"),
            via_str(*via),
            w.cwd.replacen(&home_s, "~", 1)
        );
    }
    outln!();
    outln!(
        "{:<7} {:<7} {:<18} {:<10} {:<9} SESSION",
        "PANE",
        "AGENT",
        "WORKSPACE",
        "PALETTE",
        "VIA"
    );
    let label_of: HashMap<&str, &str> = workspaces
        .iter()
        .map(|w| (w.id.as_str(), w.label.as_str()))
        .collect();
    for p in &panes {
        let (pal, via) = &a.pane_palette[&p.id];
        let session = p
            .session
            .as_deref()
            .map(|s| {
                Path::new(s)
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or(s)
            })
            .unwrap_or("-");
        outln!(
            "{:<7} {:<7} {:<18} {:<10} {:<9} {}",
            p.id,
            p.agent.as_deref().unwrap_or("shell"),
            label_of
                .get(p.workspace_id.as_str())
                .copied()
                .unwrap_or("?"),
            pal.as_deref().unwrap_or("-"),
            via_str(*via),
            session
        );
    }
    Ok(())
}

fn via_str(v: Via) -> &'static str {
    match v {
        Via::Agent => "agent",
        Via::Rule => "rule",
        Via::Auto => "auto",
        Via::None => "none",
    }
}

fn cmd_validate() -> Res<()> {
    let ctx = ctx();
    let cfg = load_plugin_config(&ctx)?;
    outln!("config: {}", plugin_config_path(&ctx).display());
    outln!("herdr config: {}", herdr_config_path(&cfg).display());
    outln!(
        "auto: {}   sidebar: {}   marker: {}   pane tint: {}",
        cfg.auto,
        cfg.sidebar,
        cfg.marker,
        cfg.pane_tint
    );
    outln!("palettes ({}):", cfg.palettes.len());
    for (name, p) in &cfg.palettes {
        let pairs: Vec<String> = p.tokens.iter().map(|(k, v)| format!("{k}={v}")).collect();
        outln!(
            "  {name}: {}{}{}",
            pairs.join(" "),
            p.pane_bg
                .as_ref()
                .map(|b| format!(" pane_bg={b}"))
                .unwrap_or_default(),
            if p.light.is_some() { " (+light)" } else { "" }
        );
    }
    outln!("workspace rules ({}):", cfg.rules.len());
    for r in &cfg.rules {
        let what = match (&r.label, &r.path) {
            (Some(l), Some(p)) => format!("label {l:?} or path {p}"),
            (Some(l), None) => format!("label {l:?}"),
            (None, Some(p)) => format!("path {p}"),
            (None, None) => "(invalid)".into(),
        };
        outln!("  {what} → {}", r.palette);
    }
    outln!("agent rules ({}):", cfg.agents.len());
    for a in &cfg.agents {
        outln!("  session {} → {}", a.session, a.palette);
    }
    outln!("config is valid.");
    Ok(())
}

fn cmd_palettes() -> Res<()> {
    let ctx = ctx();
    let cfg = load_plugin_config(&ctx)?;
    for (name, p) in &cfg.palettes {
        outln!(
            "{:<8} accent {}  sidebar_bg {}  pane {}{}",
            name,
            p.tokens.get("accent").map(String::as_str).unwrap_or("-"),
            p.tokens
                .get("sidebar_bg")
                .map(String::as_str)
                .unwrap_or("-"),
            p.pane_bg(false).unwrap_or_else(|| "-".into()),
            if p.light.is_some() { "  (+light)" } else { "" }
        );
    }
    Ok(())
}

// --- rule editing (plugin config, format-preserving) ----------------------------

fn edit_plugin_config(ctx: &Ctx, f: impl FnOnce(&mut DocumentMut) -> Res<()>) -> Res<()> {
    let path = plugin_config_path(ctx);
    let text = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f(&mut doc)?;
    let updated = doc.to_string();
    parse_plugin_config(&updated)
        .map_err(|e| format!("refusing to write an invalid config: {e}"))?;
    write_atomic(&path, &updated)
}

fn aot_mut<'a>(doc: &'a mut DocumentMut, key: &str) -> &'a mut toml_edit::ArrayOfTables {
    if doc.get(key).and_then(Item::as_array_of_tables).is_none() {
        doc.insert(key, Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    }
    doc.get_mut(key)
        .and_then(Item::as_array_of_tables_mut)
        .expect("just inserted")
}

fn upsert_rule(doc: &mut DocumentMut, field: &str, key: &str, palette: &str) {
    let list = aot_mut(doc, "workspaces");
    for t in list.iter_mut() {
        if t.get(field).and_then(Item::as_str) == Some(key) {
            t["palette"] = value(palette);
            return;
        }
    }
    let mut t = Table::new();
    t[field] = value(key);
    t["palette"] = value(palette);
    list.push(t);
}

fn remove_rule(doc: &mut DocumentMut, key: &str) -> usize {
    let Some(list) = doc
        .get_mut("workspaces")
        .and_then(Item::as_array_of_tables_mut)
    else {
        return 0;
    };
    let before = list.len();
    let keep: Vec<Table> = list
        .iter()
        .filter(|t| {
            t.get("label").and_then(Item::as_str) != Some(key)
                && t.get("path").and_then(Item::as_str) != Some(key)
        })
        .cloned()
        .collect();
    list.clear();
    for t in keep {
        list.push(t);
    }
    before - list.len()
}

fn cmd_set(args: &[String]) -> Res<()> {
    let [target, palette] = args else {
        return Err("usage: set <label|id|path> <palette>".into());
    };
    let ctx = ctx();
    let cfg = load_plugin_config(&ctx)?;
    if !cfg.palettes.contains_key(palette) {
        return Err(format!("unknown palette {palette:?}; see `palettes`"));
    }
    let (field, key) = if target.starts_with('/') || target.starts_with('~') {
        ("path", target.clone())
    } else {
        (
            "label",
            find_workspace(&ctx, target)
                .map(|w| w.label)
                .unwrap_or_else(|_| target.clone()),
        )
    };
    edit_plugin_config(&ctx, |doc| {
        upsert_rule(doc, field, &key, palette);
        Ok(())
    })?;
    outln!("[{TAG}] rule saved: {field} {key:?} → {palette}");
    cmd_apply(false)
}

fn cmd_unset(args: &[String]) -> Res<()> {
    let [target] = args else {
        return Err("usage: unset <label|path>".into());
    };
    let ctx = ctx();
    let mut removed = 0;
    edit_plugin_config(&ctx, |doc| {
        removed = remove_rule(doc, target);
        Ok(())
    })?;
    outln!("[{TAG}] removed {removed} rule(s) for {target:?}");
    cmd_apply(false)
}

fn resolve_session(ctx: &Ctx, target: &str) -> Res<(String, String)> {
    if target.contains(':') {
        let p = all_panes(ctx)?
            .into_iter()
            .find(|p| p.id == target)
            .ok_or_else(|| format!("no pane {target:?}"))?;
        let s = p
            .session
            .ok_or_else(|| format!("pane {target} has no agent session to pin"))?;
        let note = format!("{} in {}", p.agent.unwrap_or_default(), p.workspace_id);
        Ok((s, note))
    } else {
        Ok((target.to_owned(), String::new()))
    }
}

fn cmd_set_agent(args: &[String]) -> Res<()> {
    let [target, palette] = args else {
        return Err("usage: set-agent <pane-id|session> <palette>".into());
    };
    let ctx = ctx();
    let cfg = load_plugin_config(&ctx)?;
    if !cfg.palettes.contains_key(palette) {
        return Err(format!("unknown palette {palette:?}; see `palettes`"));
    }
    let (session, note) = resolve_session(&ctx, target)?;
    edit_plugin_config(&ctx, |doc| {
        let list = aot_mut(doc, "agents");
        for t in list.iter_mut() {
            if t.get("session").and_then(Item::as_str) == Some(session.as_str()) {
                t["palette"] = value(palette);
                return Ok(());
            }
        }
        let mut t = Table::new();
        t["session"] = value(&session);
        t["palette"] = value(palette);
        if !note.is_empty() {
            t["note"] = value(&note);
        }
        list.push(t);
        Ok(())
    })?;
    outln!("[{TAG}] agent rule saved: {session} → {palette}");
    cmd_sweep(false)
}

fn cmd_unset_agent(args: &[String]) -> Res<()> {
    let [target] = args else {
        return Err("usage: unset-agent <pane-id|session>".into());
    };
    let ctx = ctx();
    let (session, _) = resolve_session(&ctx, target)?;
    let mut removed = 0;
    edit_plugin_config(&ctx, |doc| {
        if let Some(list) = doc.get_mut("agents").and_then(Item::as_array_of_tables_mut) {
            let keep: Vec<Table> = list
                .iter()
                .filter(|t| t.get("session").and_then(Item::as_str) != Some(session.as_str()))
                .cloned()
                .collect();
            removed = list.len() - keep.len();
            list.clear();
            for t in keep {
                list.push(t);
            }
        }
        Ok(())
    })?;
    outln!("[{TAG}] removed {removed} agent rule(s)");
    cmd_sweep(false)
}

/// Palette choices this plugin exposes as static right-click actions. Runtime
/// action registration is not in plugin v1, so custom user palettes cannot
/// appear here — the built-in eight plus `auto` cover the menu.
const ACTION_PALETTES: &[&str] = &[
    "red", "peach", "yellow", "green", "teal", "blue", "mauve", "pink",
];

/// Emit the `[[actions]]` entries for the palette menu, so the manifest can be
/// regenerated from one source of truth (`ACTION_PALETTES`). Titlecased names;
/// workspace actions set a persisted rule, pane actions pin the pane's agent.
fn action_blocks() -> String {
    let mut out = String::new();
    for (scope, verb) in [("workspace", "Colour"), ("pane", "Pin")] {
        for p in ACTION_PALETTES {
            let title = {
                let mut c = p.chars();
                let first = c.next().unwrap().to_uppercase().collect::<String>();
                format!("{first}{}", c.as_str())
            };
            out.push_str(&format!(
                "\n[[actions]]\nid = \"{scope}-colour-{p}\"\ntitle = \"{verb}: {title}\"\ncontexts = [\"{scope}\"]\ncommand = [\"sh\", \"bin/herdr-space-colors\", \"action-colour\", \"{scope}\", \"{p}\"]\n"
            ));
        }
        let auto_title = if scope == "workspace" {
            "Colour: Auto (clear)"
        } else {
            "Pin: Clear"
        };
        out.push_str(&format!(
            "\n[[actions]]\nid = \"{scope}-colour-auto\"\ntitle = \"{auto_title}\"\ncontexts = [\"{scope}\"]\ncommand = [\"sh\", \"bin/herdr-space-colors\", \"action-colour\", \"{scope}\", \"auto\"]\n"
        ));
    }
    out
}

fn cmd_manifest_actions() -> Res<()> {
    out!("{}", action_blocks());
    Ok(())
}

/// Menu-driven colour change. Invoked by the per-palette actions herdr shows on
/// a workspace or pane right-click; `scope` is baked into each action's command
/// and the target id comes from the context herdr injects (HERDR_WORKSPACE_ID /
/// HERDR_PANE_ID). `palette` is a palette name or "auto" to drop the rule.
fn cmd_action_colour(args: &[String]) -> Res<()> {
    let [scope, palette] = args else {
        return Err("usage: action-colour <workspace|pane> <palette|auto>".into());
    };
    let auto = palette == "auto";
    let ctx = ctx();
    let cfg = load_plugin_config(&ctx)?;
    if !auto && !cfg.palettes.contains_key(palette.as_str()) {
        return Err(format!("unknown palette {palette:?}; see `palettes`"));
    }
    match scope.as_str() {
        "workspace" => {
            let id = env::var("HERDR_WORKSPACE_ID")
                .map_err(|_| "no workspace in this context".to_string())?;
            let label = find_workspace(&ctx, &id)?.label;
            edit_plugin_config(&ctx, |doc| {
                if auto {
                    remove_rule(doc, &label);
                } else {
                    upsert_rule(doc, "label", &label, palette);
                }
                Ok(())
            })?;
            notify(
                &ctx,
                &format!("{label} \u{2192} {}", if auto { "auto" } else { palette }),
            );
            cmd_apply(false)
        }
        "pane" => {
            let pane =
                env::var("HERDR_PANE_ID").map_err(|_| "no pane in this context".to_string())?;
            // A pane is pinned through its agent session; a pane with no agent
            // has nothing to pin, so report it as a toast rather than failing
            // the menu action.
            let res = if auto {
                cmd_unset_agent(std::slice::from_ref(&pane))
            } else {
                cmd_set_agent(&[pane.clone(), palette.clone()])
            };
            match res {
                Ok(()) => {
                    notify(
                        &ctx,
                        &format!(
                            "pane {pane} \u{2192} {}",
                            if auto { "auto" } else { palette }
                        ),
                    );
                    Ok(())
                }
                Err(e) => {
                    notify(&ctx, &e);
                    Ok(())
                }
            }
        }
        other => Err(format!(
            "unknown scope {other:?}; expected workspace or pane"
        )),
    }
}

fn cmd_focus(args: &[String]) -> Res<()> {
    let [target] = args else {
        return Err("usage: focus <label|id>".into());
    };
    let ctx = ctx();
    let ws = find_workspace(&ctx, target)?;
    herdr_run(&ctx, &["workspace", "focus", &ws.id])?;
    outln!("[{TAG}] focused {} ({})", ws.label, ws.id);
    Ok(())
}

// --- pane tint ------------------------------------------------------------------

fn cmd_osc(pane: Option<&str>, reset: bool) -> Res<()> {
    if reset {
        out!("\x1b]111\x07");
        return Ok(());
    }
    let ctx = ctx();
    let cfg = load_plugin_config(&ctx)?;
    let pane_id = pane
        .map(str::to_owned)
        .or_else(|| env::var("HERDR_PANE_ID").ok())
        .ok_or("no pane: pass --pane <id> or run inside a herdr pane")?;
    if !cfg.pane_tint {
        out!("\x1b]111\x07");
        return Ok(());
    }
    let path = herdr_config_path(&cfg);
    let light = fs::read_to_string(&path)
        .ok()
        .and_then(|t| t.parse::<DocumentMut>().ok())
        .is_some_and(|d| theme_mode(&d) == ThemeMode::Light);

    let panes = all_panes(&ctx)?;
    let pane = panes
        .iter()
        .find(|p| p.id == pane_id)
        .ok_or_else(|| format!("no pane {pane_id:?}"))?;
    let palette = match agent_palette(&cfg, pane.session.as_deref()) {
        Some(a) => Some(a.to_owned()),
        None => {
            let ws = find_workspace(&ctx, &pane.workspace_id)?;
            choose_palette(&cfg, &ws).0.map(str::to_owned)
        }
    };
    match palette.and_then(|n| cfg.palettes[&n].pane_bg(light)) {
        Some(bg) => out!("\x1b]11;{bg}\x07"),
        None => out!("\x1b]111\x07"),
    }
    Ok(())
}

fn shell_hook_snippet() -> String {
    format!(
        "{HOOK_MARKER}: tint this pane to its workspace colour
if [ -n \"$HERDR_PANE_ID\" ] && [ -t 1 ] && command -v herdr-space-colors >/dev/null 2>&1; then
  herdr-space-colors osc
fi
"
    )
}

fn cmd_shell_hook(write: bool) -> Res<()> {
    let snippet = shell_hook_snippet();
    if !write {
        out!("{snippet}");
        eprintln!("[{TAG}] append this to ~/.zshrc (or run `shell-hook --write`); needs `install-cli` first");
        return Ok(());
    }
    let rc = home().join(".zshrc");
    let existing = fs::read_to_string(&rc).unwrap_or_default();
    if existing.contains(HOOK_MARKER) {
        outln!("[{TAG}] hook already present in {}", rc.display());
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push('\n');
    updated.push_str(&snippet);
    fs::write(&rc, updated).map_err(|e| format!("write {}: {e}", rc.display()))?;
    outln!("[{TAG}] hook appended to {} — new panes tint themselves; existing panes keep their colour until restarted", rc.display());
    Ok(())
}

fn cmd_install_cli() -> Res<()> {
    let root = plugin_root().ok_or("cannot locate the plugin root (set HERDR_PLUGIN_ROOT)")?;
    let shim = root.join("bin/herdr-space-colors");
    if !shim.exists() {
        return Err(format!("shim not found at {}", shim.display()));
    }
    let bin_dir = home().join(".local/bin");
    fs::create_dir_all(&bin_dir).map_err(|e| format!("create {}: {e}", bin_dir.display()))?;
    let link = bin_dir.join("herdr-space-colors");
    if link.exists() || fs::symlink_metadata(&link).is_ok() {
        fs::remove_file(&link).map_err(|e| format!("replace {}: {e}", link.display()))?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&shim, &link)
        .map_err(|e| format!("symlink {}: {e}", link.display()))?;
    #[cfg(not(unix))]
    return Err("install-cli is only supported on unix".into());
    outln!("[{TAG}] linked {} → {}", link.display(), shim.display());
    if !env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|p| Path::new(p) == bin_dir)
    {
        eprintln!("[{TAG}] note: {} is not on your PATH", bin_dir.display());
    }
    Ok(())
}

// --- tests ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("space-colors-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lock_contention_retries_then_proceeds() {
        let dir = scratch_dir("lock-retry");
        let path = dir.join("apply.lock");
        let held = acquire_lock_at(&path, Duration::ZERO).unwrap().unwrap();
        let start = Instant::now();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        let got = acquire_lock_at(&path, Duration::from_secs(3)).unwrap();
        releaser.join().unwrap();
        assert!(
            got.is_some(),
            "waiter must get the lock once it is released"
        );
        assert!(start.elapsed() >= Duration::from_millis(300));
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn lock_held_past_the_wait_gives_up_and_a_stale_lock_is_taken_over() {
        let dir = scratch_dir("lock-timeout");
        let path = dir.join("apply.lock");
        let _held = acquire_lock_at(&path, Duration::ZERO).unwrap().unwrap();
        let start = Instant::now();
        assert!(acquire_lock_at(&path, Duration::from_millis(200))
            .unwrap()
            .is_none());
        assert!(start.elapsed() >= Duration::from_millis(200));

        let stale = dir.join("stale.lock");
        let f = fs::File::create(&stale).unwrap();
        f.set_modified(SystemTime::now() - LOCK_STALE * 2).unwrap();
        assert!(acquire_lock_at(&stale, Duration::ZERO).unwrap().is_some());
    }

    fn session(socket: &str) -> Session {
        Session {
            name: "test".into(),
            socket: Some(socket.into()),
            key: session_key(Some(socket), "test"),
            server: Some(format!("{socket}#1")),
        }
    }

    fn want(pairs: &[(&str, Option<&str>)]) -> BTreeMap<String, (Option<String>, Via)> {
        pairs
            .iter()
            .map(|(id, p)| (id.to_string(), (p.map(str::to_owned), Via::Auto)))
            .collect()
    }

    /// What a successful sweep records: every reported tag lands in the cache.
    fn record(st: &mut SessionState, w: &BTreeMap<String, (Option<String>, Via)>) {
        for (id, palette) in stale_tags(&mut st.ws_tokens, w) {
            st.ws_tokens.insert(id, palette.unwrap_or_default());
        }
    }

    #[test]
    fn session_key_follows_the_socket_and_falls_back_to_the_name() {
        let a = session_key(Some("/h/.config/herdr/herdr.sock"), "default");
        let b = session_key(Some("/h/.config/herdr/sessions/two/herdr.sock"), "two");
        assert_ne!(a, b);
        assert_eq!(a, session_key(Some("/h/.config/herdr/herdr.sock"), "other"));
        assert_ne!(session_key(None, "two"), session_key(None, "default"));
        assert_ne!(session_key(None, "two"), b);
        assert!(a.len() == 16 && a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sessions_with_overlapping_ids_keep_independent_caches() {
        let dir = scratch_dir("sessions-independent");
        let (a, b) = (session("/s/a.sock"), session("/s/b.sock"));
        let mut sa = load_session_state(&dir, &a);
        sa.ws_tokens.insert("w1".into(), "peach".into());
        sa.pane_tokens.insert("w1:p1".into(), "peach".into());
        let mut sb = load_session_state(&dir, &b);
        sb.ws_tokens.insert("w1".into(), "blue".into());
        sb.pane_tokens.insert("w1:p1".into(), "teal".into());
        save_session_state(&dir, &a, &sa).unwrap();
        save_session_state(&dir, &b, &sb).unwrap();
        assert_eq!(load_session_state(&dir, &a), sa);
        assert_eq!(load_session_state(&dir, &b), sb);
    }

    #[test]
    fn a_sweep_in_one_session_does_not_suppress_tags_in_another() {
        let dir = scratch_dir("sessions-sweep");
        let (a, b) = (session("/s/a.sock"), session("/s/b.sock"));

        // B tagged w3 earlier; A has no w3 at all.
        let mut sb = load_session_state(&dir, &b);
        record(&mut sb, &want(&[("w3", Some("mauve"))]));
        save_session_state(&dir, &b, &sb).unwrap();

        // A sweeps its own w1 → peach (and retains only its live ids).
        let mut sa = load_session_state(&dir, &a);
        record(&mut sa, &want(&[("w1", Some("peach"))]));
        save_session_state(&dir, &a, &sa).unwrap();

        // B's w1 also wants peach but was never tagged in B: it must be reported,
        // and B's w3 survives A's retain.
        let mut sb = load_session_state(&dir, &b);
        let stale = stale_tags(
            &mut sb.ws_tokens,
            &want(&[("w1", Some("peach")), ("w3", Some("mauve"))]),
        );
        assert_eq!(stale, vec![("w1".to_string(), Some("peach".to_string()))]);
        assert_eq!(sb.ws_tokens.get("w3").map(String::as_str), Some("mauve"));
    }

    #[test]
    fn a_restarted_server_voids_the_tag_cache() {
        let dir = scratch_dir("server-restart");
        let mut a = session("/s/a.sock");
        let mut st = load_session_state(&dir, &a);
        record(&mut st, &want(&[("w1", Some("peach"))]));
        save_session_state(&dir, &a, &st).unwrap();
        assert_eq!(load_session_state(&dir, &a).ws_tokens.len(), 1);

        // Same socket path (same key), new server instance: ids come back
        // without their tokens, so w1 must be reported again.
        a.server = Some("/s/a.sock#2".into());
        let mut st = load_session_state(&dir, &a);
        assert!(st.ws_tokens.is_empty());
        assert_eq!(st.server.as_deref(), Some("/s/a.sock#2"));
        assert_eq!(
            stale_tags(&mut st.ws_tokens, &want(&[("w1", Some("peach"))])).len(),
            1
        );
    }

    #[test]
    fn stale_tags_skips_what_is_in_place_and_drops_dead_ids() {
        let mut cache: BTreeMap<String, String> = [
            ("w1".to_string(), "peach".to_string()),
            ("w2".to_string(), String::new()),
            ("w9".to_string(), "red".to_string()),
        ]
        .into();
        let stale = stale_tags(
            &mut cache,
            &want(&[("w1", Some("peach")), ("w2", None), ("w3", None)]),
        );
        assert_eq!(stale, vec![("w3".to_string(), None)]);
        assert!(!cache.contains_key("w9"));
    }

    #[test]
    fn legacy_shared_caches_are_ignored_and_dropped_on_save() {
        let dir = scratch_dir("legacy-state");
        let ctx = Ctx {
            herdr: "herdr".into(),
            config_dir: dir.clone(),
            state_dir: dir.clone(),
        };
        fs::write(
            state_path(&ctx),
            r##"{"managed":["accent"],"managed_sidebar":["ui.sidebar.agents.rows"],
                "ws_tokens":{"w1":"peach"},"pane_tokens":{"w1:p1":"peach"},
                "last_workspace":"w1","window_original":{"ttys001":"#000000"}}"##,
        )
        .unwrap();
        let st = load_state(&ctx);
        assert_eq!(st.managed, vec!["accent"]);
        assert_eq!(
            st.managed_sidebar
                .get("ui.sidebar.agents.rows")
                .map(String::as_str),
            Some(""),
            "a list from 0.3.3 means managed, fingerprint unknown"
        );
        assert_eq!(
            st.window.original.get("ttys001").map(String::as_str),
            Some("#000000")
        );
        let fresh = load_session_state(&dir, &session("/s/a.sock"));
        assert!(fresh.ws_tokens.is_empty() && fresh.pane_tokens.is_empty());

        save_state(&ctx, &st).unwrap();
        let text = fs::read_to_string(state_path(&ctx)).unwrap();
        assert!(!text.contains("ws_tokens") && !text.contains("pane_tokens"));
        assert!(text.contains("accent"));
    }

    #[test]
    fn herdr_dirs_follow_xdg_like_herdr_and_fall_back_to_home() {
        use std::ffi::OsStr;
        let home = Path::new("/h");
        assert_eq!(
            herdr_dir(Some(OsStr::new("/x/cfg")), home, ".config"),
            Path::new("/x/cfg/herdr")
        );
        assert_eq!(
            herdr_dir(None, home, ".config"),
            Path::new("/h/.config/herdr")
        );
        assert_eq!(
            herdr_dir(Some(OsStr::new("")), home, ".local/state"),
            Path::new("/h/.local/state/herdr")
        );
    }

    #[test]
    fn manifest_lists_every_generated_palette_action() {
        let manifest = include_str!("../herdr-plugin.toml");
        for block in action_blocks()
            .split("[[actions]]")
            .filter(|b| b.contains("id ="))
        {
            let id = block
                .lines()
                .find_map(|l| l.trim().strip_prefix("id = \""))
                .and_then(|l| l.strip_suffix('"'))
                .expect("generated block has an id");
            assert!(
                manifest.contains(&format!("id = \"{id}\"")),
                "manifest is missing action {id}; run `manifest-actions` and update herdr-plugin.toml"
            );
        }
        // every palette gets a workspace and a pane action, plus two auto entries
        assert_eq!(
            action_blocks().matches("[[actions]]").count(),
            ACTION_PALETTES.len() * 2 + 2
        );
    }

    fn tokens(pairs: &[(&str, &str)]) -> Tokens {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn ws(label: &str, cwd: &str) -> Workspace {
        Workspace {
            id: "w".into(),
            label: label.into(),
            cwd: cwd.into(),
            focused: false,
        }
    }

    const USER_CONFIG: &str = "\
# my herdr config
onboarding = false

[session]
resume_agents_on_restore = true   # keep

[ui]
agent_panel_sort = \"priority\"

[[keys.command]]
key = \"prefix+space\"
type = \"plugin_action\"
command = \"herdr-layout-cycle.cycle-layout\"
";

    #[test]
    fn apply_adds_keys_and_preserves_everything_else() {
        let out = plan_apply(
            USER_CONFIG,
            &tokens(&[("accent", "#f38ba8"), ("sidebar_bg", "#2e1f26")]),
            &[],
        )
        .unwrap();
        assert!(out.starts_with("# my herdr config\n"));
        assert!(out.contains("resume_agents_on_restore = true   # keep"));
        assert!(out.contains("agent_panel_sort = \"priority\""));
        assert!(out.contains("[theme.custom]"));
        assert!(out.contains("accent = \"#f38ba8\""));
        assert!(!out.contains("\n[theme]\n"));
    }

    #[test]
    fn apply_is_idempotent_and_switching_removes_stale_keys() {
        let p = tokens(&[("accent", "#f38ba8"), ("sidebar_bg", "#2e1f26")]);
        let once = plan_apply(USER_CONFIG, &p, &[]).unwrap();
        let twice = plan_apply(&once, &p, &["accent".into(), "sidebar_bg".into()]).unwrap();
        assert_eq!(once, twice);
        let third = plan_apply(
            &twice,
            &tokens(&[("accent", "#89b4fa")]),
            &["accent".into(), "sidebar_bg".into()],
        )
        .unwrap();
        assert!(third.contains("accent = \"#89b4fa\"") && !third.contains("sidebar_bg"));
    }

    #[test]
    fn clear_restores_original_bytes_including_sidebar_rows() {
        let spec = SidebarSpec {
            colours: vec![
                ("red".into(), "#f38ba8".into()),
                ("blue".into(), "#89b4fa".into()),
            ],
        };
        let p = tokens(&[("accent", "#f38ba8")]);
        let plan = Plan {
            theme: Some(&p),
            previously_managed: &[],
            sidebar: Some(&spec),
            managed_sidebar: &BTreeMap::new(),
            legacy: &[],
        };
        let Planned {
            text: applied,
            managed_sidebar: managed,
            ..
        } = plan_full(USER_CONFIG, &plan).unwrap();
        assert_eq!(managed.keys().collect::<Vec<_>>(), ["agents", "spaces"]);
        assert!(applied.contains("[ui.sidebar.agents]"), "{applied}");
        assert!(
            applied.contains("token = \"$sc_red\", fg = \"#f38ba8\", bold = true"),
            "{applied}"
        );
        assert!(applied.contains("[ui.sidebar.spaces]"));
        assert!(
            !applied.contains("\n[ui.sidebar]\n"),
            "intermediate table must stay implicit"
        );
        assert!(
            applied.contains("agent_panel_sort = \"priority\""),
            "user's [ui] keys must survive"
        );
        let cleared = plan_clear(&applied, &["accent".into()], &managed, &[]).unwrap();
        assert_eq!(cleared.text, USER_CONFIG);
    }

    #[test]
    fn user_defined_sidebar_rows_are_never_touched() {
        let base = format!("{USER_CONFIG}\n[ui.sidebar.agents]\nrows = [[\"agent\"]]\n");
        let spec = SidebarSpec {
            colours: vec![("red".into(), "#f38ba8".into())],
        };
        let plan = Plan {
            theme: None,
            previously_managed: &[],
            sidebar: Some(&spec),
            managed_sidebar: &BTreeMap::new(),
            legacy: &[],
        };
        let Planned {
            text: applied,
            managed_sidebar: managed,
            ..
        } = plan_full(&base, &plan).unwrap();
        assert_eq!(
            managed.keys().collect::<Vec<_>>(),
            ["spaces"],
            "only the absent kind is managed"
        );
        assert!(
            applied.contains("rows = [[\"agent\"]]"),
            "user rows preserved verbatim"
        );
        let cleared = plan_clear(&applied, &[], &managed, &[]).unwrap();
        assert_eq!(cleared.text, base);
    }

    fn spec_of(pairs: &[(&str, &str)]) -> SidebarSpec {
        SidebarSpec {
            colours: pairs
                .iter()
                .map(|(n, c)| (n.to_string(), c.to_string()))
                .collect(),
        }
    }

    fn apply_rows(text: &str, spec: &SidebarSpec, managed: &BTreeMap<String, String>) -> Planned {
        let plan = Plan {
            theme: None,
            previously_managed: &[],
            sidebar: Some(spec),
            managed_sidebar: managed,
            legacy: std::slice::from_ref(spec),
        };
        plan_full(text, &plan).unwrap()
    }

    const CREW_ROWS: &str =
        "rows = [[\"state_icon\", \"$crew_top_peach\", \"$crew_tab\"], [\"state_text\"]]";

    #[test]
    fn rows_replaced_after_a_write_survive_apply_focus_and_clear() {
        let spec = spec_of(&[("red", "#f38ba8"), ("blue", "#89b4fa")]);
        let first = apply_rows(USER_CONFIG, &spec, &BTreeMap::new());
        assert_eq!(first.managed_sidebar.len(), 2);

        // Another tool replaces the agents rows the plugin wrote.
        let doc: DocumentMut = first.text.parse().unwrap();
        let mut doc = doc;
        doc["ui"]["sidebar"]["agents"]["rows"] = toml_edit::Item::None;
        let theirs = doc.to_string().replace(
            "[ui.sidebar.agents]\n",
            &format!("[ui.sidebar.agents]\n{CREW_ROWS}\n"),
        );
        assert!(theirs.contains(CREW_ROWS), "{theirs}");

        // Next focus: agents released and untouched, spaces still managed.
        let second = apply_rows(&theirs, &spec, &first.managed_sidebar);
        assert_eq!(second.released, ["agents"]);
        assert_eq!(
            second.managed_sidebar.keys().collect::<Vec<_>>(),
            ["spaces"]
        );
        assert!(second.text.contains(CREW_ROWS));
        assert_eq!(second.text, theirs);

        // A later focus no longer considers them, and does not re-report.
        let third = apply_rows(&second.text, &spec, &second.managed_sidebar);
        assert!(third.released.is_empty());
        assert!(third.text.contains(CREW_ROWS));

        // Clear removes only the spaces rows.
        let cleared = plan_clear(&third.text, &[], &third.managed_sidebar, &[]).unwrap();
        assert!(cleared.text.contains(CREW_ROWS));
        assert!(!cleared.text.contains("[ui.sidebar.spaces]"));

        // Clear straight after the replacement (no apply in between) also keeps them.
        let cleared = plan_clear(&theirs, &[], &first.managed_sidebar, &[]).unwrap();
        assert_eq!(cleared.released, ["agents"]);
        assert!(cleared.text.contains(CREW_ROWS));
        assert!(!cleared.text.contains("[ui.sidebar.spaces]"));
    }

    #[test]
    fn untouched_managed_rows_still_follow_palette_changes() {
        let dark = spec_of(&[("red", "#f38ba8")]);
        let light = spec_of(&[("red", "#d20f39")]);
        let first = apply_rows(USER_CONFIG, &dark, &BTreeMap::new());
        let second = apply_rows(&first.text, &light, &first.managed_sidebar);
        assert!(second.released.is_empty());
        assert_eq!(second.managed_sidebar.len(), 2);
        assert!(second.text.contains("#d20f39") && !second.text.contains("#f38ba8"));
        // Reformatting the plugin's own rows does not count as a change.
        let reformatted = second.text.replace(", ", ",  ");
        let third = apply_rows(&reformatted, &light, &second.managed_sidebar);
        assert!(third.released.is_empty());
        let cleared = plan_clear(&third.text, &[], &third.managed_sidebar, &[]).unwrap();
        assert_eq!(cleared.text, USER_CONFIG);
    }

    #[test]
    fn rows_without_a_fingerprint_are_kept_only_if_they_match_what_would_be_written() {
        let spec = spec_of(&[("red", "#f38ba8")]);
        let ours = apply_rows(USER_CONFIG, &spec, &BTreeMap::new()).text;
        let legacy: BTreeMap<String, String> = [
            ("agents".to_string(), String::new()),
            ("spaces".to_string(), String::new()),
        ]
        .into();
        let kept = apply_rows(&ours, &spec, &legacy);
        assert!(kept.released.is_empty());
        assert!(kept.managed_sidebar.values().all(|fp| !fp.is_empty()));

        let theirs = ours.replacen("rows = [[\"state_icon\"", "rows = [[\"agent\"", 1);
        let planned = apply_rows(&theirs, &spec, &legacy);
        assert_eq!(planned.released, ["agents"]);
        assert_eq!(planned.text, theirs);
    }

    #[test]
    fn user_owned_custom_keys_survive() {
        let base = format!(
            "{USER_CONFIG}\n[theme]\nname = \"catppuccin\"\n\n[theme.custom]\ntext = \"#ffffff\"\n"
        );
        let applied = plan_apply(&base, &tokens(&[("accent", "#f38ba8")]), &[]).unwrap();
        let cleared = plan_clear(&applied, &["accent".into()], &BTreeMap::new(), &[])
            .unwrap()
            .text;
        assert!(cleared.contains("text = \"#ffffff\"") && !cleared.contains("accent"));
        assert!(cleared.contains("[theme]\nname = \"catppuccin\""));
    }

    #[test]
    fn inline_custom_table_is_handled() {
        let base = "[theme]\ncustom = { text = \"#ffffff\" }\n";
        let applied = plan_apply(base, &tokens(&[("accent", "#f38ba8")]), &[]).unwrap();
        assert!(applied.contains("accent = \"#f38ba8\"") && applied.contains("text = \"#ffffff\""));
        let cleared = plan_clear(&applied, &["accent".into()], &BTreeMap::new(), &[])
            .unwrap()
            .text;
        assert!(cleared.contains("text = \"#ffffff\"") && !cleared.contains("accent"));
    }

    #[test]
    fn hex_validation_is_strict() {
        for ok in ["#fff", "#F38BA8", "#000000"] {
            assert!(is_hex_colour(ok), "{ok}");
        }
        for bad in [
            "fff",
            "#ff",
            "#ffff",
            "#ggg",
            "red",
            "rgb(1,2,3)",
            "#f38ba8 ",
        ] {
            assert!(!is_hex_colour(bad), "{bad}");
        }
    }

    #[test]
    fn plugin_config_validation() {
        assert!(parse_plugin_config("[palettes.x]\nnope = \"#fff\"\n")
            .unwrap_err()
            .contains("not a theme.custom token"));
        assert!(parse_plugin_config("[palettes.x]\naccent = \"red\"\n")
            .unwrap_err()
            .contains("#rgb or #rrggbb"));
        assert!(parse_plugin_config("[palettes.x]\naccent = \"#fff\"\n[[workspaces]]\nlabel = \"a\"\npalette = \"missing\"\n").unwrap_err().contains("not a defined palette"));
        assert!(parse_plugin_config(
            "[palettes.x]\naccent = \"#fff\"\n[[agents]]\nsession = \"s\"\npalette = \"missing\"\n"
        )
        .unwrap_err()
        .contains("not a defined palette"));
        assert!(parse_plugin_config(
            "[sidebar]\nenabled = true\n[palettes.x]\nsidebar_bg = \"#fff\"\n"
        )
        .unwrap_err()
        .contains("needs `accent`"));
        let cfg =
            parse_plugin_config(DEFAULT_CONFIG).expect("shipped default config must validate");
        assert!(
            cfg.palettes.values().all(|p| p.light.is_some()),
            "every shipped palette has a light variant"
        );
    }

    #[test]
    fn light_variant_overlays_base_and_pane_bg_falls_back_to_sidebar_bg() {
        let cfg = parse_plugin_config(
            "[palettes.red]\naccent = \"#f38ba8\"\nsidebar_bg = \"#2e1f26\"\n[palettes.red.light]\naccent = \"#d20f39\"\nsidebar_bg = \"#f2dfe3\"\n",
        ).unwrap();
        let p = &cfg.palettes["red"];
        assert_eq!(p.effective(false)["accent"], "#f38ba8");
        assert_eq!(p.effective(true)["accent"], "#d20f39");
        assert_eq!(p.pane_bg(false).unwrap(), "#2e1f26");
        assert_eq!(p.pane_bg(true).unwrap(), "#f2dfe3");
    }

    #[test]
    fn theme_mode_detection() {
        let d = |s: &str| theme_mode(&s.parse::<DocumentMut>().unwrap());
        assert_eq!(d(""), ThemeMode::Dark);
        assert_eq!(
            d("[theme]\nname = \"catppuccin-latte\"\n"),
            ThemeMode::Light
        );
        assert_eq!(d("[theme]\nname = \"rose-pine-dawn\"\n"), ThemeMode::Light);
        assert_eq!(
            d("[theme]\nname = \"dracula\"\nauto_switch = true\n"),
            ThemeMode::Auto
        );
    }

    #[test]
    fn selection_precedence_label_then_longest_path_then_auto_and_agent_override() {
        let cfg = parse_plugin_config(
            "[palettes.a]\naccent = \"#111\"\n[palettes.b]\naccent = \"#222\"\n[palettes.c]\naccent = \"#333\"\n\
             [[workspaces]]\npath = \"/p\"\npalette = \"a\"\n\
             [[workspaces]]\npath = \"/p/deep\"\npalette = \"b\"\n\
             [[workspaces]]\nlabel = \"Exact\"\npalette = \"c\"\n\
             [[agents]]\nsession = \"abc-123\"\npalette = \"c\"\n",
        ).unwrap();
        assert!(matches!(
            choose_palette(&cfg, &ws("Exact", "/p/deep/x")),
            (Some("c"), Via::Rule)
        ));
        assert!(matches!(
            choose_palette(&cfg, &ws("x", "/p/deep/x")),
            (Some("b"), Via::Rule)
        ));
        assert!(
            matches!(choose_palette(&cfg, &ws("x", "/plain")), (_, Via::Auto)),
            "no false prefix match"
        );
        assert_eq!(agent_palette(&cfg, Some("abc-123")), Some("c"));
        assert_eq!(
            agent_palette(&cfg, Some("/home/x/.pi/sessions/2026_abc-123.jsonl")),
            Some("c"),
            "pi path suffix"
        );
        assert_eq!(agent_palette(&cfg, Some("other")), None);
        assert_eq!(agent_palette(&cfg, None), None);
    }

    #[test]
    fn assignment_uses_agent_override_over_workspace() {
        let cfg = parse_plugin_config(
            "[palettes.a]\naccent = \"#111\"\n[palettes.b]\naccent = \"#222\"\n[[workspaces]]\nlabel = \"W\"\npalette = \"a\"\n[[agents]]\nsession = \"s1\"\npalette = \"b\"\n",
        ).unwrap();
        let w = Workspace {
            id: "w1".into(),
            label: "W".into(),
            cwd: "/w".into(),
            focused: true,
        };
        let panes = vec![
            Pane {
                id: "w1:p1".into(),
                workspace_id: "w1".into(),
                agent: Some("claude".into()),
                session: Some("s1".into()),
            },
            Pane {
                id: "w1:p2".into(),
                workspace_id: "w1".into(),
                agent: None,
                session: None,
            },
        ];
        let a = assign(&cfg, &[w], &panes);
        assert_eq!(a.pane_palette["w1:p1"], (Some("b".into()), Via::Agent));
        assert_eq!(a.pane_palette["w1:p2"], (Some("a".into()), Via::Rule));
        assert_eq!(a.ws_palette["w1"], (Some("a".into()), Via::Rule));
    }

    #[test]
    fn report_args_set_one_token_and_clear_the_rest() {
        let all = vec!["red".to_string(), "green".to_string()];
        let args = report_args("pane", "w1:p1", Some("green"), "●", &all);
        assert_eq!(
            args,
            vec![
                "pane",
                "report-metadata",
                "w1:p1",
                "--source",
                "space-colors",
                "--clear-token",
                "sc_red",
                "--token",
                "sc_green=●"
            ]
        );
        let none = report_args("workspace", "w1", None, "●", &all);
        assert!(none.iter().all(|a| a != "--token"));
    }

    #[test]
    fn rule_upsert_and_remove_preserve_comments() {
        let mut doc: DocumentMut = "# keep me\nauto = true\n\n[palettes.x]\naccent = \"#fff\"\n"
            .parse()
            .unwrap();
        upsert_rule(&mut doc, "label", "A", "x");
        upsert_rule(&mut doc, "path", "/p", "x");
        upsert_rule(&mut doc, "label", "A", "x");
        let s = doc.to_string();
        assert!(s.starts_with("# keep me\n"));
        assert_eq!(s.matches("[[workspaces]]").count(), 2, "{s}");
        assert_eq!(remove_rule(&mut doc, "A"), 1);
        assert_eq!(doc.to_string().matches("[[workspaces]]").count(), 1);
        parse_plugin_config(&doc.to_string()).unwrap();
    }

    #[test]
    fn shell_hook_is_guarded() {
        let s = shell_hook_snippet();
        assert!(s.contains(HOOK_MARKER) && s.contains("HERDR_PANE_ID") && s.contains("[ -t 1 ]"));
    }
}
