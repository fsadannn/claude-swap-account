//! swap-core: back up and switch the account state of **Claude Desktop** (MSIX app on Windows),
//! one account active at a time. No GUI, no async – the `claude-swap` app is a thin layer on top.
//!
//! **Current scope: Claude Desktop only.** Claude Code (`~/.claude`, `~/.claude.json`) support is written and
//! tested but switched off (`Settings::manage_code == false`) and not exposed in the GUI; while it is off
//! nothing under `~/.claude` / `~/.claude.json` is read, copied or modified. See the README TODO.
//!
//! Layout of the store (`%LOCALAPPDATA%\claude-swap`):
//! ```text
//! accounts/<name>/desktop/<entries...>  snapshot of the Desktop data folder (switch scope)
//! accounts/<name>/code/...              only present when Claude Code support is enabled
//! accounts/<name>/meta.json             name, e-mail, format, has_desktop / has_code
//! backups/<account>/<ms>-<reason>.tar.gz  backup of the live state taken before any overwrite and on demand
//!                                         (backup scope = switch scope + session data);
//!                                         <account> = active account, else "<email>_unassigned" / "_unassigned";
//!                                         newest N kept per folder (see `Swapper::max_backups`)
//! settings.json                         user settings (`Settings`)
//! state.json                            { "active": "<name>" }
//! pending.json                          crash-recovery marker (live state may be half-written)
//! ```
//!
//! Two scopes (`Scope`): **Switch** = what makes up an account (sign-in, MCP config, extensions);
//! **Backup** = Switch + session data (+ git data if enabled). Both are "whole folder minus excludes", so
//! files Anthropic adds later are picked up automatically; `Swapper::scan_large` warns when something big shows up.
//! Restoring a backup puts back everything inside the archive and never deletes live data that is not in it.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Snapshot format. v2 = "whole folder minus excludes" (v1 was a short allow-list and is not restorable).
pub const FORMAT: u32 = 2;

/// Items of the Desktop data dir that carry the *sign-in* (removed by "sign out" when adding an account).
/// `config.json` holds `oauth:tokenCache` (encrypted with the key in `Local State`),
/// `Network/Cookies` holds the claude.ai session.
const DESKTOP_IDENTITY: &[&str] = &[
    "config.json", "Local State", "Preferences", "ant-did", "ant-device-registry.json",
    "Network", "Local Storage", "Session Storage", "IndexedDB", "WebStorage",
];
/// Must exist for the Desktop to count as signed in.
const REQUIRED: &[&str] = &["config.json", "Local State"];

/// Top-level entries of the Desktop data dir that are NOT part of an account snapshot:
/// caches, huge VM images, logs, and machine-local sessions. EVERYTHING ELSE is swapped
/// (sign-in, MCP config `claude_desktop_config.json`, `Claude Extensions`, ...).
pub const DESKTOP_EXCLUDE: &[&str] = &[
    "vm_bundles", "Cache", "Code Cache", "GPUCache", "DawnGraphiteCache", "DawnWebGPUCache",
    "ShaderCache", "GrShaderCache", "component_crx_cache", "fcache", "Crashpad", "logs", "sentry",
    "Shared Dictionary", "blob_storage",
    "claude-code", "claude-code-sessions", "claude-code-vm", "git-shadow", "scratch-workspaces",
    "pending-uploads", "local-agent-mode-sessions",
    "sessiondata.vhdx", "git-worktrees.json", "window-state.json",
    "declarative_performance_observer.db", "declarative_performance_observer.db-journal",
];
/// Claude Code (inactive for now, see module docs): top-level entries of `~/.claude` that are NOT swapped
/// (session transcripts, caches, history). Everything else (credentials, settings.json, skills/, commands/,
/// agents/, plugins/, CLAUDE.md, ...) would be swapped.
pub const CODE_EXCLUDE: &[&str] = &[
    "projects", "todos", "shell-snapshots", "statsig", "file-history", "debug", "telemetry",
    "cache", "ide", "session-env", "paste-cache", "image-cache", "plans", "backups",
    "history.jsonl", ".last-cleanup",
];
/// Backups are broader than switching: they ALSO include the session data that a switch leaves alone
/// (Cowork `local-agent-mode-sessions`, `claude-code-sessions`, git helpers, and for Claude Code
/// `projects`, `todos`, `plans`, `history.jsonl`). Still excluded: caches, VM images/disks, logs, the
/// downloaded `claude-code` binaries and other disposable data.
pub const DESKTOP_BACKUP_EXCLUDE: &[&str] = &[
    "vm_bundles", "Cache", "Code Cache", "GPUCache", "DawnGraphiteCache", "DawnWebGPUCache",
    "ShaderCache", "GrShaderCache", "component_crx_cache", "fcache", "Crashpad", "logs", "sentry",
    "Shared Dictionary", "blob_storage",
    "claude-code", "claude-code-vm", "sessiondata.vhdx",
    "declarative_performance_observer.db", "declarative_performance_observer.db-journal",
];
/// Desktop git data: skipped in backups unless `Settings::backup_git` is on (never part of a switch).
pub const DESKTOP_GIT_ITEMS: &[&str] = &["git-shadow", "git-worktrees.json", "scratch-workspaces"];
pub const CODE_BACKUP_EXCLUDE: &[&str] = &[
    "shell-snapshots", "statsig", "file-history", "debug", "telemetry",
    "cache", "ide", "session-env", "paste-cache", "image-cache", "backups", ".last-cleanup",
];

/// What a snapshot/restore covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// account switching: sign-in, MCP, skills, settings (sessions are left alone)
    Switch,
    /// backups: everything in `Switch` plus session data
    Backup,
}

/// Claude Code (inactive for now): keys of `~/.claude.json` that would be swapped (the rest – project history
/// etc. – is left alone). `mcpServers` = user-scope MCP servers of Claude Code.
pub const CODE_JSON_KEYS: &[&str] = &["oauthAccount", "mcpServers"];

/// Never copied at any depth (Chromium lock/singleton files).
const SKIP_FILES: &[&str] = &[
    "SingletonLock", "SingletonCookie", "SingletonSocket", "lockfile", "LOCK",
];
/// Default number of backup archives kept in each account's backup folder
/// (override with `Swapper::with_max_backups(Some(n))` or `max_backups` in settings.json).
pub const DEFAULT_MAX_BACKUPS_PER_ACCOUNT: usize = 15;
const CODE_CREDS: &str = ".credentials.json";
const OAUTH_KEY: &str = "oauthAccount";
const DEFAULT_THRESHOLD_MB: u64 = 100;

#[derive(Clone, Debug)]
pub struct Paths {
    /// Claude Desktop user-data dir (MSIX virtualised Roaming\Claude, or %APPDATA%\Claude).
    pub desktop: PathBuf,
    /// ~/.claude (only used when Claude Code support is enabled)
    pub code_dir: PathBuf,
    /// ~/.claude.json (only used when Claude Code support is enabled)
    pub code_json: PathBuf,
    /// our store
    pub store: PathBuf,
}

impl Paths {
    /// Auto-detect on the current machine (Windows).
    pub fn detect() -> Result<Paths> {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("no home dir"))?;
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        let roaming = std::env::var_os("APPDATA").map(PathBuf::from);
        let desktop = find_desktop_dir(local.as_deref(), roaming.as_deref())
            .ok_or_else(|| anyhow!("Claude Desktop data folder not found (is Claude Desktop installed and was it started once?)"))?;
        let store = local.unwrap_or_else(|| home.clone()).join("claude-swap");
        Ok(Paths {
            desktop,
            code_dir: home.join(".claude"),
            code_json: home.join(".claude.json"),
            store,
        })
    }
}

/// Resolution order:
/// 1) Packages\<Claude pkg>\LocalCache\Roaming\Claude  2) %APPDATA%\Claude
pub fn find_desktop_dir(local: Option<&Path>, roaming: Option<&Path>) -> Option<PathBuf> {
    if let Some(local) = local {
        let pk = local.join("Packages");
        if let Ok(rd) = fs::read_dir(&pk) {
            let mut c: Vec<PathBuf> = rd
                .flatten()
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    n.starts_with("Claude_") || n.starts_with("AnthropicPBC.Claude_")
                })
                .map(|e| e.path().join("LocalCache").join("Roaming").join("Claude"))
                .filter(|p| p.is_dir())
                .collect();
            c.sort();
            if let Some(p) = c.pop() {
                return Some(p);
            }
        }
    }
    roaming.map(|r| r.join("Claude")).filter(|p| p.is_dir())
}

// ---------------------------------------------------------------- model

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Meta {
    pub name: String,
    pub email: Option<String>,
    pub saved_at_ms: u128,
    #[serde(default)]
    pub format: u32,
    /// which parts this snapshot contains (Desktop data folder / Claude Code files)
    #[serde(default = "yes")]
    pub has_desktop: bool,
    #[serde(default = "yes")]
    pub has_code: bool,
}

fn yes() -> bool { true }

/// User settings stored in `<store>/settings.json`.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    /// warn when a not-excluded top-level item is bigger than this
    pub threshold_mb: u64,
    /// extra excludes, "desktop:<name>" or "code:<name>"
    pub exclude: Vec<String>,
    /// big items the user decided to keep (no more warnings), same key format
    pub acknowledged: Vec<String>,
    /// backups kept per account folder; `None` = built-in default
    pub max_backups: Option<usize>,
    /// also put Desktop's git data (`DESKTOP_GIT_ITEMS`, can be large) into backups
    pub backup_git: bool,
    /// handle Claude Desktop (its data folder) in backups / account switching
    pub manage_desktop: bool,
    /// handle Claude Code (`~/.claude` + `~/.claude.json`: CLI, and Desktop's Code tab) in backups / switching.
    /// OFF by default and not exposed in the GUI yet (planned, see README TODO). While off, nothing under
    /// `~/.claude` / `~/.claude.json` is read, copied or modified.
    pub manage_code: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Settings { threshold_mb: DEFAULT_THRESHOLD_MB, exclude: vec![], acknowledged: vec![], max_backups: None, backup_git: false, manage_desktop: true, manage_code: false }
    }
}

/// Result of a manual backup.
#[derive(Serialize, Clone, Debug)]
pub struct BackupReport {
    pub archive: String,
    /// files that could not be read (locked by a running Claude)
    pub skipped: Vec<String>,
    /// true if Claude was running (best-effort backup)
    pub live: bool,
    pub size_bytes: u64,
}

/// A big top-level item that would be included in every backup / account snapshot.
#[derive(Serialize, Clone, Debug)]
pub struct LargeItem {
    /// "desktop" or "code"
    pub root: String,
    pub name: String,
    pub bytes: u64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Status {
    pub desktop_dir: String,
    pub store_dir: String,
    pub claude_running: bool,
    pub active: Option<String>,
    /// e-mail of the signed-in account, read from Claude Desktop's IndexedDB blobs (plus
    /// ~/.claude.json only if Claude Code support is enabled); `None` if not found
    pub live_email: Option<String>,
    pub desktop_signed_in: bool,
    pub pending_recovery: bool,
    pub accounts: Vec<Meta>,
    pub settings: Settings,
    /// effective backup limit per account (after parameter/settings/default resolution)
    pub max_backups: usize,
    pub backups_dir: String,
}

pub struct Swapper {
    pub paths: Paths,
    /// returns true if Claude Desktop is running (injected so tests don't need processes)
    pub is_running: Box<dyn Fn() -> bool + Send + Sync>,
    /// explicit backup limit per account; `None` = use settings.json, else the default
    max_backups: Option<usize>,
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// Read the signed-in e-mail from Claude Desktop's data dir. The claude.ai web app stores the
/// account object in IndexedDB blobs (`IndexedDB/https_claude.ai_0.indexeddb.blob/**`) as V8
/// serialised data: `... email_address " <len> <utf8 bytes> ...`. Blob file numbers are not stable,
/// so every file is scanned and the most recently modified match wins. Read-only and tolerant of
/// files held open by a running Claude; any failure just means "unknown" (None).
fn desktop_email(desktop: &Path) -> Option<String> {
    let root = desktop.join("IndexedDB").join("https_claude.ai_0.indexeddb.blob");
    let mut best: Option<(SystemTime, String)> = None;
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() { stack.push(p); continue; }
            if md.len() > 5_000_000 { continue; }
            let Ok(bytes) = fs::read(&p) else { continue };
            if let Some(m) = email_in_blob(&bytes) {
                let t = md.modified().unwrap_or(UNIX_EPOCH);
                if best.as_ref().map_or(true, |(bt, _)| t > *bt) { best = Some((t, m)); }
            }
        }
    }
    best.map(|(_, m)| m)
}

/// Find `email_address` followed by a V8 one-byte string (`"` + varint length + bytes) that looks
/// like an e-mail address.
fn email_in_blob(b: &[u8]) -> Option<String> {
    const KEY: &[u8] = b"email_address";
    let mut i = 0;
    while i + KEY.len() + 3 < b.len() {
        if &b[i..i + KEY.len()] == KEY {
            let mut j = i + KEY.len();
            if b[j] == b'"' {
                j += 1;
                let (mut len, mut shift) = (0usize, 0);
                while j < b.len() && shift < 21 {
                    let c = b[j];
                    j += 1;
                    len |= ((c & 0x7f) as usize) << shift;
                    shift += 7;
                    if c & 0x80 == 0 { break; }
                }
                if (3..=254).contains(&len) && j + len <= b.len() {
                    if let Ok(s) = std::str::from_utf8(&b[j..j + len]) {
                        let at = s.find('@');
                        if at.map_or(false, |a| a > 0 && s[a..].contains('.') ) && !s.chars().any(|c| c.is_whitespace() || c.is_control()) {
                            return Some(s.to_string());
                        }
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// Normalise an e-mail into a safe, readable folder-name part: trimmed, lower-case, `@` becomes
/// `_at_`, every other character outside `a-z 0-9 . -` becomes `_`, runs of `_` are collapsed and
/// leading/trailing `_` `.` are removed (so `Frank.S+test@Gmail.com` -> `frank.s_test_at_gmail.com`).
/// Never empty: falls back to `unknown`.
fn sanitize_email(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().to_lowercase().chars() {
        let piece = match c {
            '@' => "_at_".to_string(),
            c if c.is_ascii_alphanumeric() || c == '.' || c == '-' => c.to_string(),
            _ => "_".to_string(),
        };
        for ch in piece.chars() {
            if ch == '_' && out.ends_with('_') { continue; }
            out.push(ch);
        }
    }
    let t = out.trim_matches(|c| c == '_' || c == '.');
    if t.is_empty() { "unknown".into() } else { t.into() }
}

/// file-name safe version of a free text reason
fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

pub fn validate_name(n: &str) -> Result<()> {
    let ok = !n.is_empty()
        && n.len() <= 40
        && !n.starts_with('.')
        && n.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '_' | '-' | '.'));
    if ok { Ok(()) } else { bail!("invalid account name (letters, digits, space, _ - . ; max 40)") }
}

// ---------------------------------------------------------------- fs helpers

/// Copy a tree. With `lenient`, files that cannot be read (e.g. locked by a running Claude)
/// are skipped and listed in `skipped` instead of failing the whole copy.
fn copy_tree(src: &Path, dst: &Path, lenient: bool, skipped: &mut Vec<String>) -> Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        let rd = match fs::read_dir(src) {
            Ok(r) => r,
            Err(e) if lenient => { skipped.push(format!("{} ({e})", src.display())); return Ok(()); }
            Err(e) => return Err(e.into()),
        };
        for e in rd {
            let e = match e { Ok(e) => e, Err(_) if lenient => continue, Err(e) => return Err(e.into()) };
            let name = e.file_name().to_string_lossy().to_string();
            if SKIP_FILES.contains(&name.as_str()) { continue; }
            copy_tree(&e.path(), &dst.join(&name), lenient, skipped)?;
        }
    } else {
        if let Some(p) = dst.parent() { fs::create_dir_all(p)?; }
        if let Err(e) = fs::copy(src, dst) {
            if lenient {
                skipped.push(format!("{} ({e})", src.display()));
                let _ = fs::remove_file(dst);
            } else {
                return Err(anyhow::Error::from(e).context(format!("copy {} -> {}", src.display(), dst.display())));
            }
        }
    }
    Ok(())
}

fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            if SKIP_FILES.contains(&name.as_str()) {
                continue;
            }
            copy_recursive(&e.path(), &dst.join(&name))?;
        }
    } else {
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        fs::copy(src, dst).with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    }
    Ok(())
}

/// Recursive size, not following symlinks, ignoring lock files.
fn dir_size(p: &Path) -> u64 {
    let Ok(md) = fs::symlink_metadata(p) else { return 0 };
    if md.is_file() { return md.len(); }
    if !md.is_dir() { return 0; }
    fs::read_dir(p)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| !SKIP_FILES.contains(&e.file_name().to_string_lossy().as_ref()))
        .map(|e| dir_size(&e.path()))
        .sum()
}

/// All top-level entry names of a directory (lock files excluded).
fn list_entries(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| !SKIP_FILES.contains(&n.as_str()))
        .collect()
}

fn remove_any(p: &Path) -> Result<()> {
    if p.is_dir() {
        fs::remove_dir_all(p).with_context(|| format!("remove {}", p.display()))?;
    } else if p.exists() {
        fs::remove_file(p).with_context(|| format!("remove {}", p.display()))?;
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", now_ms()));
    fs::write(&tmp, bytes)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn read_json_obj(p: &Path) -> Option<Map<String, Value>> {
    let s = fs::read_to_string(p).ok()?;
    match serde_json::from_str::<Value>(&s).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

// ---------------------------------------------------------------- impl

impl Swapper {
    pub fn new(paths: Paths, is_running: Box<dyn Fn() -> bool + Send + Sync>) -> Self {
        Swapper { paths, is_running, max_backups: None }
    }

    /// Set how many backups to keep per account. `None` = fall back to settings.json, then the default.
    pub fn with_max_backups(mut self, max: Option<usize>) -> Self {
        self.max_backups = max;
        self
    }

    /// Effective limit: explicit parameter > settings.json > default. Never less than 1.
    pub fn max_backups(&self) -> usize {
        self.max_backups
            .or(self.settings().max_backups)
            .unwrap_or(DEFAULT_MAX_BACKUPS_PER_ACCOUNT)
            .max(1)
    }

    fn accounts_dir(&self) -> PathBuf { self.paths.store.join("accounts") }
    fn backups_dir(&self) -> PathBuf { self.paths.store.join("backups") }
    fn state_file(&self) -> PathBuf { self.paths.store.join("state.json") }
    fn pending_file(&self) -> PathBuf { self.paths.store.join("pending.json") }
    fn account_dir(&self, n: &str) -> PathBuf { self.accounts_dir().join(n) }

    fn ensure_closed(&self) -> Result<()> {
        if (self.is_running)() {
            bail!("Claude Desktop is running – close it first (or use 'Close Claude').");
        }
        Ok(())
    }

    pub fn active(&self) -> Option<String> {
        read_json_obj(&self.state_file())
            .and_then(|m| m.get("active").and_then(|v| v.as_str().map(String::from)))
    }

    fn set_active(&self, a: Option<&str>) -> Result<()> {
        fs::create_dir_all(&self.paths.store)?;
        let v = serde_json::json!({ "active": a });
        write_atomic(&self.state_file(), serde_json::to_string_pretty(&v)?.as_bytes())
    }

    /// E-mail of whoever is signed in. Claude Desktop keeps it (plain text, V8-serialised) in the
    /// claude.ai IndexedDB blobs; Claude Code's `~/.claude.json` is only consulted when that part is
    /// enabled (`manage_code`), so by default nothing under `~/.claude*` is read.
    fn live_email(&self) -> Option<String> {
        let st = self.settings();
        if st.manage_desktop {
            if let Some(m) = desktop_email(&self.paths.desktop) { return Some(m); }
        }
        if !st.manage_code { return None; }
        read_json_obj(&self.paths.code_json)?
            .get(OAUTH_KEY)?
            .get("emailAddress")?
            .as_str()
            .map(String::from)
    }

    /// Signed in to something we manage: Desktop sign-in files and/or (if enabled) Claude Code credentials.
    fn desktop_signed_in(&self) -> bool {
        let st = self.settings();
        (st.manage_desktop && REQUIRED.iter().all(|r| self.paths.desktop.join(r).exists()))
            || (st.manage_code && self.paths.code_dir.join(CODE_CREDS).exists())
    }

    pub fn list(&self) -> Vec<Meta> {
        let mut v: Vec<Meta> = fs::read_dir(self.accounts_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { return None; }
                let m = fs::read_to_string(e.path().join("meta.json"))
                    .ok()
                    .and_then(|s| serde_json::from_str::<Meta>(&s).ok());
                Some(m.unwrap_or(Meta { name, ..Default::default() }))
            })
            .collect();
        v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        v
    }

    pub fn status(&self) -> Status {
        Status {
            desktop_dir: self.paths.desktop.display().to_string(),
            store_dir: self.paths.store.display().to_string(),
            claude_running: (self.is_running)(),
            active: self.active(),
            live_email: self.live_email(),
            desktop_signed_in: self.desktop_signed_in(),
            pending_recovery: self.pending_file().exists(),
            accounts: self.list(),
            settings: self.settings(),
            max_backups: self.max_backups(),
            backups_dir: self.backups_dir().display().to_string(),
        }
    }

    // ---- snapshot (live -> dir) and restore (dir -> live)

    // ---- what is managed

    pub fn settings(&self) -> Settings {
        fs::read_to_string(self.paths.store.join("settings.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save_settings(&self, st: &Settings) -> Result<()> {
        fs::create_dir_all(&self.paths.store)?;
        write_atomic(&self.paths.store.join("settings.json"), serde_json::to_string_pretty(st)?.as_bytes())
    }

    fn is_excluded(root: &str, name: &str, st: &Settings, scope: Scope) -> bool {
        let builtin = match (root == "desktop", scope) {
            (true, Scope::Switch) => DESKTOP_EXCLUDE,
            (true, Scope::Backup) => DESKTOP_BACKUP_EXCLUDE,
            (false, Scope::Switch) => CODE_EXCLUDE,
            (false, Scope::Backup) => CODE_BACKUP_EXCLUDE,
        };
        let key = format!("{root}:{name}").to_lowercase();
        let git_off = scope == Scope::Backup
            && root == "desktop"
            && !st.backup_git
            && DESKTOP_GIT_ITEMS.iter().any(|e| e.eq_ignore_ascii_case(name));
        git_off
            || builtin.iter().any(|e| e.eq_ignore_ascii_case(name))
            || SKIP_FILES.iter().any(|e| e.eq_ignore_ascii_case(name))
            || st.exclude.iter().any(|e| e.to_lowercase() == key)
    }

    /// Top-level entries of `dir` that are swapped for `root`.
    fn managed(&self, root: &str, dir: &Path, st: &Settings, scope: Scope) -> Vec<String> {
        fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| !Self::is_excluded(root, n, st, scope))
            .collect()
    }

    /// Scan for big top-level items that are included in backups/snapshots (call on demand – it walks the disk).
    pub fn scan_large(&self) -> Vec<LargeItem> {
        let st = self.settings();
        let limit = st.threshold_mb.saturating_mul(1024 * 1024);
        let mut out = vec![];
        for (root, dir) in [("desktop", &self.paths.desktop), ("code", &self.paths.code_dir)] {
            if (root == "desktop" && !st.manage_desktop) || (root == "code" && !st.manage_code) { continue; }
            for n in self.managed(root, dir, &st, Scope::Backup) {
                if st.acknowledged.iter().any(|a| a.eq_ignore_ascii_case(&format!("{root}:{n}"))) { continue; }
                let bytes = dir_size(&dir.join(&n));
                if bytes > limit { out.push(LargeItem { root: root.into(), name: n, bytes }); }
            }
        }
        out.sort_by(|a, b| b.bytes.cmp(&a.bytes));
        out
    }

    /// `None` = use the built-in default.
    pub fn set_max_backups(&self, n: Option<usize>) -> Result<()> {
        let mut st = self.settings();
        st.max_backups = n.map(|v| v.max(1));
        self.save_settings(&st)
    }

    /// Choose what is backed up / switched (Desktop / Claude Code). At least one must stay on.
    /// Claude Code is off by default and has no GUI switch yet.
    pub fn set_targets(&self, desktop: bool, code: bool) -> Result<()> {
        if !desktop && !code { bail!("enable at least one of Claude Desktop / Claude Code"); }
        let mut st = self.settings();
        st.manage_desktop = desktop;
        st.manage_code = code;
        self.save_settings(&st)
    }

    pub fn set_backup_git(&self, on: bool) -> Result<()> {
        let mut st = self.settings();
        st.backup_git = on;
        self.save_settings(&st)
    }

    pub fn set_threshold_mb(&self, mb: u64) -> Result<()> {
        let mut st = self.settings();
        st.threshold_mb = mb;
        self.save_settings(&st)
    }

    /// Include an item again (undo `exclude_item`). `key` is "desktop:<name>" / "code:<name>".
    pub fn remove_exclude(&self, key: &str) -> Result<()> {
        let mut st = self.settings();
        st.exclude.retain(|k| k != key);
        self.save_settings(&st)
    }

    /// Warn again about an item (undo `acknowledge_item`).
    pub fn forget_acknowledged(&self, key: &str) -> Result<()> {
        let mut st = self.settings();
        st.acknowledged.retain(|k| k != key);
        self.save_settings(&st)
    }

    /// Stop swapping/backing up this item (it stays untouched on disk).
    pub fn exclude_item(&self, root: &str, name: &str) -> Result<()> {
        let mut st = self.settings();
        let k = format!("{root}:{name}");
        if !st.exclude.contains(&k) { st.exclude.push(k); }
        self.save_settings(&st)
    }

    /// Keep including this item and stop warning about it.
    pub fn acknowledge_item(&self, root: &str, name: &str) -> Result<()> {
        let mut st = self.settings();
        let k = format!("{root}:{name}");
        if !st.acknowledged.contains(&k) { st.acknowledged.push(k); }
        self.save_settings(&st)
    }

    // ---- snapshot (live -> dir) and restore (dir -> live)

    fn snapshot_to(&self, dir: &Path, name: &str) -> Result<()> {
        self.snapshot_impl(dir, name, Scope::Switch, false, &mut vec![])
    }

    fn snapshot_impl(&self, dir: &Path, name: &str, scope: Scope, lenient: bool, skipped: &mut Vec<String>) -> Result<()> {
        let st = self.settings();
        let d = dir.join("desktop");
        fs::create_dir_all(&d)?;
        if st.manage_desktop {
            for n in self.managed("desktop", &self.paths.desktop, &st, scope) {
                copy_tree(&self.paths.desktop.join(&n), &d.join(&n), lenient, skipped)?;
            }
        }
        let c = dir.join("code").join("claude");
        fs::create_dir_all(&c)?;
        if st.manage_code {
            for n in self.managed("code", &self.paths.code_dir, &st, scope) {
                copy_tree(&self.paths.code_dir.join(&n), &c.join(&n), lenient, skipped)?;
            }
            let mut keys = Map::new();
            if let Some(m) = read_json_obj(&self.paths.code_json) {
                for k in CODE_JSON_KEYS {
                    if let Some(v) = m.get(*k) { keys.insert((*k).into(), v.clone()); }
                }
            }
            fs::write(dir.join("code").join("claude-json.json"), serde_json::to_string_pretty(&keys)?)?;
        }
        let meta = Meta { name: name.to_string(), email: self.live_email(), saved_at_ms: now_ms(), format: FORMAT, has_desktop: st.manage_desktop, has_code: st.manage_code };
        fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
        Ok(())
    }

    fn restore_from(&self, dir: &Path, scope: Scope) -> Result<()> {
        let meta: Meta = fs::read_to_string(dir.join("meta.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        if meta.format < FORMAT {
            bail!("this snapshot was saved by an older version – sign in to that account and save it again");
        }
        let st = self.settings();
        // switch: only the parts that are turned on now AND exist in the snapshot;
        // explicit backup restore: every part that exists in the archive.
        let (do_desktop, do_code) = if scope == Scope::Backup {
            (meta.has_desktop, meta.has_code)
        } else {
            (meta.has_desktop && st.manage_desktop, meta.has_code && st.manage_code)
        };
        // What gets removed from the live folders / copied from the snapshot:
        //  * Switch: the managed (account) entries only.
        //  * Backup (explicit restore): EVERYTHING that is inside the archive, whatever today's exclude
        //    settings are. Each such entry is replaced as a whole; live entries that are not in the
        //    archive are never deleted.
        let full = scope == Scope::Backup;
        let plan = |root: &str, live: &Path, snap: &Path| -> (Vec<String>, Vec<String>) {
            let copy: Vec<String> = if full { list_entries(snap) } else { self.managed(root, snap, &st, Scope::Switch) };
            let remove = if full { copy.clone() } else { self.managed(root, live, &st, Scope::Switch) };
            (remove, copy)
        };
        // Claude Desktop data dir
        if do_desktop {
            fs::create_dir_all(&self.paths.desktop)?;
            let d = dir.join("desktop");
            let (remove, copy) = plan("desktop", &self.paths.desktop, &d);
            for n in remove { remove_any(&self.paths.desktop.join(&n))?; }
            for n in copy { copy_recursive(&d.join(&n), &self.paths.desktop.join(&n))?; }
        }
        if !do_code { return Ok(()); }
        // ~/.claude (credentials, settings, skills, sessions in a full restore, ...)
        fs::create_dir_all(&self.paths.code_dir)?;
        let c = dir.join("code").join("claude");
        let (remove, copy) = plan("code", &self.paths.code_dir, &c);
        for n in remove { remove_any(&self.paths.code_dir.join(&n))?; }
        for n in copy { copy_recursive(&c.join(&n), &self.paths.code_dir.join(&n))?; }
        // ~/.claude.json: merge only the swapped keys, keep everything else
        let saved: Map<String, Value> = read_json_obj(&dir.join("code").join("claude-json.json")).unwrap_or_default();
        self.merge_code_json(&saved)
    }

    /// Set/remove exactly `CODE_JSON_KEYS` in ~/.claude.json according to `saved`.
    fn merge_code_json(&self, saved: &Map<String, Value>) -> Result<()> {
        let existing = read_json_obj(&self.paths.code_json);
        let mut m = existing.clone().unwrap_or_default();
        for k in CODE_JSON_KEYS {
            match saved.get(*k) {
                Some(v) => { m.insert((*k).into(), v.clone()); }
                None => { m.remove(*k); }
            }
        }
        if existing.is_some() || !m.is_empty() { self.write_code_json(&m)?; }
        Ok(())
    }

    /// Sign out of the enabled targets (Desktop, and Claude Code if on) but keep MCP config / extensions /
    /// settings in place so a newly added account starts from the current setup.
    fn sign_out_live(&self) -> Result<()> {
        let st = self.settings();
        if st.manage_desktop {
            for it in DESKTOP_IDENTITY { remove_any(&self.paths.desktop.join(it))?; }
        }
        if st.manage_code {
            remove_any(&self.paths.code_dir.join(CODE_CREDS))?;
            if let Some(mut m) = read_json_obj(&self.paths.code_json) {
                if m.remove(OAUTH_KEY).is_some() { self.write_code_json(&m)?; }
            }
        }
        Ok(())
    }

    fn write_code_json(&self, m: &Map<String, Value>) -> Result<()> {
        write_atomic(&self.paths.code_json, serde_json::to_string_pretty(m)?.as_bytes())
    }

    /// Back up the live state (always before overwriting it) with `Scope::Backup`, as
    /// `backups/<account>/<millis>-<reason>.tar.gz` where `<account>` is the active account, else
    /// `<email>_unassigned` (or `_unassigned`). Prunes old archives. Returns the archive path.
    fn backup_live(&self, reason: &str) -> Result<PathBuf> {
        Ok(self.backup_impl(reason, false)?.0)
    }

    fn backup_impl(&self, reason: &str, lenient: bool) -> Result<(PathBuf, Vec<String>)> {
        let mut skipped: Vec<String> = vec![];
        // Folder = active account's name. With no saved account active the backup is still labelled by
        // whoever is signed in: "<email>_unassigned" (or plain "_unassigned" if that is unknown).
        let owner = self.active().unwrap_or_else(|| match self.live_email() {
            Some(m) => format!("{}_unassigned", sanitize_email(&m)),
            None => "_unassigned".to_string(),
        });
        let folder = self.backups_dir().join(&owner);
        fs::create_dir_all(&folder)?;
        let stem = format!("{}-{}", now_ms(), sanitize(reason));
        let tmp = self.paths.store.join(format!(".tmp-bk-{stem}"));
        remove_any(&tmp)?;
        let archive = folder.join(format!("{stem}.tar.gz"));
        let res = (|| -> Result<()> {
            self.snapshot_impl(&tmp, "backup", Scope::Backup, lenient, &mut skipped)?;
            let part = folder.join(format!("{stem}.tar.gz.part"));
            {
                let f = fs::File::create(&part)?;
                let gz = flate2::write::GzEncoder::new(f, flate2::Compression::default());
                let mut tar = tar::Builder::new(gz);
                tar.append_dir_all(".", &tmp)?;
                tar.into_inner()?.finish()?;
            }
            fs::rename(&part, &archive)?;
            Ok(())
        })();
        let _ = remove_any(&tmp);
        res.with_context(|| format!("creating backup {}", archive.display()))?;
        self.prune_backups(&folder);
        Ok((archive, skipped))
    }

    fn prune_backups(&self, folder: &Path) {
        let mut v: Vec<PathBuf> = fs::read_dir(folder)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".tar.gz"))
            .collect();
        v.sort(); // names start with epoch millis -> oldest first
        let max = self.max_backups();
        while v.len() > max {
            let _ = fs::remove_file(v.remove(0));
        }
    }

    /// Manual backup of the CURRENT state. Works while Claude is running: files Claude holds
    /// locked are skipped (listed in the report) and SQLite/LevelDB files may be mid-write, so a
    /// live backup is best-effort. With Claude closed it is an exact, strict backup.
    pub fn backup_now(&self) -> Result<BackupReport> {
        let live = (self.is_running)();
        let (archive, skipped) = self.backup_impl(if live { "manual-live" } else { "manual" }, live)?;
        let size_bytes = fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
        Ok(BackupReport { archive: archive.display().to_string(), skipped, live, size_bytes })
    }

    /// All backup archives, newest first, as (account-folder, path).
    pub fn list_backups(&self) -> Vec<(String, PathBuf)> {
        let mut out = vec![];
        for d in fs::read_dir(self.backups_dir()).into_iter().flatten().flatten() {
            if !d.path().is_dir() { continue; }
            let owner = d.file_name().to_string_lossy().to_string();
            for f in fs::read_dir(d.path()).into_iter().flatten().flatten() {
                if f.path().to_string_lossy().ends_with(".tar.gz") {
                    out.push((owner.clone(), f.path()));
                }
            }
        }
        out.sort_by(|a, b| b.1.file_name().cmp(&a.1.file_name()));
        out
    }

    /// Restore a backup archive (Claude must be closed): puts back EVERYTHING inside it, whatever the current
    /// exclude / git / target settings are, and never deletes live entries that are not in the archive.
    pub fn restore_backup(&self, archive: &Path) -> Result<()> {
        self.ensure_closed()?;
        // an explicit restore of a backup brings back everything in it, session data included
        self.restore_from_archive(archive, Scope::Backup)
    }

    fn restore_from_archive(&self, archive: &Path, scope: Scope) -> Result<()> {
        let tmp = self.paths.store.join(format!(".tmp-rs-{}", now_ms()));
        remove_any(&tmp)?;
        fs::create_dir_all(&tmp)?;
        let res = (|| -> Result<()> {
            let f = fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
            tar::Archive::new(flate2::read::GzDecoder::new(f)).unpack(&tmp)?;
            if !tmp.join("desktop").is_dir() { bail!("not a valid backup: {}", archive.display()); }
            self.restore_from(&tmp, scope)
        })();
        let _ = remove_any(&tmp);
        res
    }

    fn mark_pending(&self, backup: &Path) -> Result<()> {
        fs::create_dir_all(&self.paths.store)?;
        let v = serde_json::json!({ "backup": backup.display().to_string() });
        write_atomic(&self.pending_file(), v.to_string().as_bytes())
    }
    fn clear_pending(&self) { let _ = fs::remove_file(self.pending_file()); }

    /// Save the live state into slot `name` (build in temp dir, then swap in).
    fn save_slot(&self, name: &str) -> Result<()> {
        let tmp = self.accounts_dir().join(format!(".tmp-{}", now_ms()));
        fs::create_dir_all(&tmp)?;
        let r = self.snapshot_to(&tmp, name);
        if let Err(e) = r { let _ = fs::remove_dir_all(&tmp); return Err(e); }
        let dst = self.account_dir(name);
        remove_any(&dst)?;
        fs::rename(&tmp, &dst)?;
        Ok(())
    }

    // ---- public operations

    /// "Save current login as <name>". Use after signing in to an account.
    pub fn save_current(&self, name: &str, overwrite: bool) -> Result<()> {
        validate_name(name)?;
        self.ensure_closed()?;
        if !self.desktop_signed_in() {
            bail!("Claude Desktop does not look signed in (config.json / Local State missing).");
        }
        if self.account_dir(name).exists() && !overwrite {
            bail!("an account named '{name}' already exists");
        }
        self.save_slot(name)?;
        self.set_active(Some(name))
    }

    /// Switch to saved account `name`: saves the current one back to its slot first
    /// (so refreshed tokens are kept), backs up live files, then restores.
    pub fn switch_to(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        self.ensure_closed()?;
        let target = self.account_dir(name);
        if !target.join("desktop").is_dir() {
            bail!("no saved account named '{name}'");
        }
        if let Some(a) = self.active() {
            if a != name && self.desktop_signed_in() && self.account_dir(&a).exists() {
                self.save_slot(&a)?;
            }
        }
        let backup = self.backup_live(&format!("switch-to-{name}"))?;
        self.mark_pending(&backup)?;
        match self.restore_from(&target, Scope::Switch) {
            Ok(()) => { self.clear_pending(); self.set_active(Some(name)) }
            Err(e) => {
                // roll back from the backup we just took
                let rb = self.restore_from_archive(&backup, Scope::Switch);
                if rb.is_ok() { self.clear_pending(); }
                Err(e.context(if rb.is_ok() { "switch failed, previous state restored" } else { "switch failed AND rollback failed – use Recover" }))
            }
        }
    }

    /// Prepare to log in to a NEW account: stores the current account (if active),
    /// backs up and clears the sign-in files. Then start Claude, sign in, and call `save_current`.
    pub fn begin_new_account(&self) -> Result<()> {
        self.ensure_closed()?;
        if let Some(a) = self.active() {
            if self.desktop_signed_in() && self.account_dir(&a).exists() {
                self.save_slot(&a)?;
            }
        }
        let backup = self.backup_live("before-new-account")?;
        self.mark_pending(&backup)?;
        self.sign_out_live()?;
        self.set_active(None)?;
        // pending stays until save_current() or recover() so a crash/cancel can be undone
        Ok(())
    }

    /// Call after a successful `save_current` following `begin_new_account`.
    pub fn finish_new_account(&self) { self.clear_pending(); }

    /// Undo: restore the live state from the backup referenced by the pending marker.
    pub fn recover(&self) -> Result<String> {
        self.ensure_closed()?;
        let p = read_json_obj(&self.pending_file()).ok_or_else(|| anyhow!("nothing to recover"))?;
        let b = PathBuf::from(p.get("backup").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("bad marker"))?);
        if !b.is_file() { bail!("backup missing: {}", b.display()); }
        self.restore_from_archive(&b, Scope::Switch)?;
        self.clear_pending();
        // active = the account whose email matches, if we can tell
        let email = self.live_email();
        let act = self.list().into_iter().find(|m| m.email.is_some() && m.email == email).map(|m| m.name);
        self.set_active(act.as_deref())?;
        Ok(b.display().to_string())
    }

    pub fn delete_account(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        remove_any(&self.account_dir(name))?;
        if self.active().as_deref() == Some(name) { self.set_active(None)?; }
        Ok(())
    }

    pub fn rename_account(&self, old: &str, new: &str) -> Result<()> {
        validate_name(old)?; validate_name(new)?;
        if self.account_dir(new).exists() { bail!("'{new}' already exists"); }
        fs::rename(self.account_dir(old), self.account_dir(new))?;
        let mp = self.account_dir(new).join("meta.json");
        if let Some(mut m) = read_json_obj(&mp) { m.insert("name".into(), new.into()); fs::write(mp, serde_json::to_string_pretty(&m)?)?; }
        if self.active().as_deref() == Some(old) { self.set_active(Some(new))?; }
        Ok(())
    }
}

// ---------------------------------------------------------------- open folder

/// Open a folder in the system file manager (creates it if missing).
pub fn open_folder(p: &Path) -> Result<()> {
    fs::create_dir_all(p)?;
    #[cfg(windows)]
    { std::process::Command::new("explorer.exe").arg(p).spawn()?; }
    #[cfg(target_os = "macos")]
    { std::process::Command::new("open").arg(p).spawn()?; }
    #[cfg(all(unix, not(target_os = "macos")))]
    { std::process::Command::new("xdg-open").arg(p).spawn()?; }
    Ok(())
}

impl Swapper {
    /// Where this app keeps accounts, backups and settings.
    pub fn store_dir(&self) -> &Path { &self.paths.store }
    pub fn open_store_dir(&self) -> Result<()> { open_folder(&self.paths.store) }
    pub fn open_backups_dir(&self) -> Result<()> { open_folder(&self.backups_dir()) }
}

// ---------------------------------------------------------------- processes / launch

/// Is `exe` the Claude *Desktop* app (not the Claude Code CLI, which is also claude.exe)?
pub fn is_desktop_exe(exe: &Path) -> bool {
    let s = exe.to_string_lossy().to_lowercase();
    s.ends_with("claude.exe") && (s.contains("windowsapps") || s.contains("anthropicclaude") || s.contains("anthropicpbc.claude"))
}

fn desktop_pids() -> Vec<sysinfo::Pid> {
    use sysinfo::{ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys.processes()
        .iter()
        .filter(|(_, p)| p.exe().map(is_desktop_exe).unwrap_or(false))
        .map(|(pid, _)| *pid)
        .collect()
}

pub fn desktop_running() -> bool { !desktop_pids().is_empty() }

/// Kill all Claude Desktop processes and wait (max ~10 s) until they are gone.
pub fn kill_desktop() -> Result<usize> {
    use sysinfo::{ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let mut n = 0;
    for (_, p) in sys.processes() {
        if p.exe().map(is_desktop_exe).unwrap_or(false) { if p.kill() { n += 1; } }
    }
    for _ in 0..40 {
        if !desktop_running() { return Ok(n); }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    bail!("Claude Desktop processes are still running")
}

#[cfg(windows)]
pub fn launch_desktop(desktop_dir: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    const NO_WINDOW: u32 = 0x0800_0000;
    // package family name = folder name under ...\Packages\
    let pfn = desktop_dir.components().map(|c| c.as_os_str().to_string_lossy().to_string())
        .find(|c| c.starts_with("Claude_") || c.starts_with("AnthropicPBC.Claude_"));
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", "(Get-StartApps | Where-Object { $_.Name -eq 'Claude' } | Select-Object -First 1).AppID"])
        .creation_flags(NO_WINDOW)
        .output();
    let mut id = out.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    if id.is_empty() {
        id = format!("{}!Claude", pfn.ok_or_else(|| anyhow!("cannot determine Claude AUMID"))?);
    }
    Command::new("explorer.exe").arg(format!("shell:AppsFolder\\{id}")).spawn()?;
    Ok(())
}

#[cfg(not(windows))]
pub fn launch_desktop(_d: &Path) -> Result<()> { bail!("launch is only implemented on Windows") }

// ---------------------------------------------------------------- high-level flows (used by the GUI)

impl Swapper {
    /// Build a Swapper for this machine (auto-detected paths, real process check).
    pub fn detect() -> Result<Swapper> {
        Ok(Swapper::new(Paths::detect()?, Box::new(desktop_running)))
    }

    /// Same as `detect`, with an optional backup limit per account (`None` = default / settings.json).
    pub fn detect_with(max_backups: Option<usize>) -> Result<Swapper> {
        Ok(Self::detect()?.with_max_backups(max_backups))
    }

    fn close_if_needed(&self, close_first: bool) -> Result<()> {
        if close_first && (self.is_running)() {
            kill_desktop()?;
        }
        Ok(())
    }

    /// Switch account and reopen Claude Desktop.
    pub fn switch_and_launch(&self, name: &str, close_first: bool) -> Result<()> {
        self.close_if_needed(close_first)?;
        self.switch_to(name)?;
        launch_desktop(&self.paths.desktop)
    }

    /// Store the current account, sign Claude out and open it so a new account can log in.
    pub fn start_new_account(&self, close_first: bool) -> Result<()> {
        self.close_if_needed(close_first)?;
        self.begin_new_account()?;
        launch_desktop(&self.paths.desktop)
    }

    /// Save the (new) login under `name` and clear the recovery marker.
    pub fn save_login(&self, name: &str, overwrite: bool) -> Result<()> {
        self.save_current(name, overwrite)?;
        self.finish_new_account();
        Ok(())
    }

    /// Close Claude (if running) and undo an interrupted switch / add-account.
    pub fn recover_closing(&self, close_first: bool) -> Result<String> {
        self.close_if_needed(close_first)?;
        self.recover()
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::{AtomicBool, Ordering}, Arc};

    struct Env { _t: tempfile::TempDir, sw: Swapper, run: Arc<AtomicBool> }

    fn w(p: &Path, s: &str) { fs::create_dir_all(p.parent().unwrap()).unwrap(); fs::write(p, s).unwrap(); }
    fn r(p: &Path) -> String { fs::read_to_string(p).unwrap_or_else(|_| "<missing>".into()) }

    fn env() -> Env {
        let t = tempfile::tempdir().unwrap();
        let b = t.path();
        let run = Arc::new(AtomicBool::new(false));
        let r2 = run.clone();
        let paths = Paths { desktop: b.join("desk"), code_dir: b.join("home/.claude"), code_json: b.join("home/.claude.json"), store: b.join("store") };
        let sw = Swapper::new(paths, Box::new(move || r2.load(Ordering::SeqCst)));
        // the library tests exercise Desktop + Claude Code; the shipped default is Desktop only
        sw.set_targets(true, true).unwrap();
        Env { _t: t, sw, run }
    }

    fn login(e: &Env, who: &str) {
        let p = &e.sw.paths;
        w(&p.desktop.join("config.json"), &format!("cfg-{who}"));
        w(&p.desktop.join("Local State"), &format!("ls-{who}"));
        w(&p.desktop.join("Network/Cookies"), &format!("ck-{who}"));
        w(&p.desktop.join("Network/lockfile"), "x"); // must be skipped
        w(&p.desktop.join("Cache/junk"), "junk"); // not in item list
        w(&p.desktop.join("claude_desktop_config.json"), &format!("mcp-desktop-{who}"));
        w(&p.desktop.join("Claude Extensions/ext/manifest.json"), &format!("ext-{who}"));
        w(&p.desktop.join("vm_bundles/rootfs.vhdx"), "HUGE-VM"); // excluded: shared, never touched
        w(&p.desktop.join("local-agent-mode-sessions/s1/x.json"), &format!("cowork-{who}")); // in backups, not in switches
        w(&p.desktop.join("claude-code-sessions/s1/y.json"), &format!("ccsess-{who}"));
        w(&p.code_dir.join(".credentials.json"), &format!("cred-{who}"));
        w(&p.code_dir.join("settings.json"), &format!("settings-{who}"));
        w(&p.code_dir.join("skills/s1/SKILL.md"), &format!("skill-{who}"));
        w(&p.code_dir.join("projects/p/t.jsonl"), "transcript"); // excluded: shared
        w(&p.code_json, &format!(r#"{{"numStartups":5,"projects":{{"a":1}},"mcpServers":{{"srv":"{who}"}},"oauthAccount":{{"emailAddress":"{who}@x.com"}}}}"#));
    }

    #[test]
    fn roundtrip_switch() {
        let e = env();
        login(&e, "alice");
        e.sw.save_current("Alice", false).unwrap();
        assert!(!e.sw.account_dir("Alice").join("desktop/Network/lockfile").exists());
        assert!(!e.sw.account_dir("Alice").join("desktop/Cache").exists());
        e.sw.begin_new_account().unwrap();
        assert!(!e.sw.desktop_signed_in());
        assert!(!e.sw.paths.code_dir.join(".credentials.json").exists());
        login(&e, "bob");
        w(&e.sw.paths.code_json, r#"{"numStartups":9,"mcpServers":{"srv":"bob"},"oauthAccount":{"emailAddress":"bob@x.com"}}"#);
        e.sw.save_current("Bob", false).unwrap();
        e.sw.finish_new_account();
        assert_eq!(e.sw.active().as_deref(), Some("Bob"));

        // token refreshed in live bob before switching -> must be kept in Bob's slot
        w(&e.sw.paths.desktop.join("config.json"), "cfg-bob-refreshed");
        e.sw.switch_to("Alice").unwrap();
        assert_eq!(r(&e.sw.paths.desktop.join("config.json")), "cfg-alice");
        assert_eq!(r(&e.sw.paths.code_dir.join(".credentials.json")), "cred-alice");
        let cj = r(&e.sw.paths.code_json);
        assert!(cj.contains("alice@x.com") && cj.contains("\"numStartups\": 9"), "{cj}");
        assert_eq!(e.sw.active().as_deref(), Some("Alice"));
        // MCP + skills + settings switched too
        assert_eq!(r(&e.sw.paths.desktop.join("claude_desktop_config.json")), "mcp-desktop-alice");
        assert_eq!(r(&e.sw.paths.desktop.join("Claude Extensions/ext/manifest.json")), "ext-alice");
        assert_eq!(r(&e.sw.paths.code_dir.join("skills/s1/SKILL.md")), "skill-alice");
        assert_eq!(r(&e.sw.paths.code_dir.join("settings.json")), "settings-alice");
        assert!(cj.contains("\"srv\": \"alice\""), "{cj}");
        // excluded items untouched
        assert_eq!(r(&e.sw.paths.desktop.join("vm_bundles/rootfs.vhdx")), "HUGE-VM");
        assert_eq!(r(&e.sw.paths.code_dir.join("projects/p/t.jsonl")), "transcript");
        e.sw.switch_to("Bob").unwrap();
        assert_eq!(r(&e.sw.paths.desktop.join("config.json")), "cfg-bob-refreshed");
        assert!(r(&e.sw.paths.code_json).contains("bob@x.com"));
        assert!(!e.sw.status().pending_recovery);
    }

    #[test]
    fn backups_are_targz_per_account_and_restorable() {
        let e = env();
        login(&e, "alice");
        e.sw.save_current("Alice", false).unwrap();
        e.sw.begin_new_account().unwrap(); // backup belongs to active = Alice
        let b = e.sw.list_backups();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].0, "Alice");
        assert!(b[0].1.to_string_lossy().ends_with(".tar.gz"));
        assert!(b[0].1.is_file());
        // no leftover temp dirs
        assert!(fs::read_dir(&e.sw.paths.store).unwrap().flatten().all(|d| !d.file_name().to_string_lossy().starts_with(".tmp-")));
        // wipe + restore from archive
        login(&e, "zed");
        e.sw.restore_backup(&b[0].1).unwrap();
        assert_eq!(r(&e.sw.paths.desktop.join("config.json")), "cfg-alice");
        assert_eq!(r(&e.sw.paths.code_dir.join(".credentials.json")), "cred-alice");
    }

    #[test]
    fn prunes_old_backups_default_setting_and_param() {
        let mk = |e: &Env, n: usize| for i in 0..n {
            e.sw.backup_live(&format!("t{i}")).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        // default
        let e = env();
        login(&e, "alice");
        assert_eq!(e.sw.max_backups(), DEFAULT_MAX_BACKUPS_PER_ACCOUNT);
        mk(&e, DEFAULT_MAX_BACKUPS_PER_ACCOUNT + 4);
        assert_eq!(e.sw.list_backups().len(), DEFAULT_MAX_BACKUPS_PER_ACCOUNT);
        let first = e.sw.list_backups()[0].0.clone();
        assert_eq!(first, "alice_at_x.com_unassigned"); // identity prefix from the signed-in e-mail
        // nobody signed in at all -> plain label
        let e2 = env();
        w(&e2.sw.paths.desktop.join("config.json"), "c");
        e2.sw.backup_live("t").unwrap();
        assert_eq!(e2.sw.list_backups()[0].0, "_unassigned");
        // settings.json value
        let e = env();
        login(&e, "alice");
        e.sw.save_settings(&Settings { max_backups: Some(5), manage_code: true, ..Default::default() }).unwrap();
        mk(&e, 8);
        assert_eq!(e.sw.list_backups().len(), 5);
        // explicit parameter wins over settings; 0 is clamped to 1
        let e = env();
        login(&e, "alice");
        e.sw.save_settings(&Settings { max_backups: Some(5), manage_code: true, ..Default::default() }).unwrap();
        let sw = Swapper::new(e.sw.paths.clone(), Box::new(|| false)).with_max_backups(Some(3));
        for i in 0..6 { sw.backup_live(&format!("p{i}")).unwrap(); std::thread::sleep(std::time::Duration::from_millis(2)); }
        assert_eq!(sw.list_backups().len(), 3);
        assert_eq!(Swapper::new(e.sw.paths.clone(), Box::new(|| false)).with_max_backups(Some(0)).max_backups(), 1);
    }

    #[test]
    fn snapshot_excludes_junk_and_old_format_is_rejected() {
        let e = env();
        login(&e, "alice");
        e.sw.save_current("Alice", false).unwrap();
        let a = e.sw.account_dir("Alice");
        assert!(a.join("desktop/claude_desktop_config.json").exists());
        assert!(a.join("code/claude/skills/s1/SKILL.md").exists());
        assert!(!a.join("desktop/vm_bundles").exists());
        assert!(!a.join("desktop/Cache").exists());
        assert!(!a.join("code/claude/projects").exists());
        // pretend it was saved by v1
        fs::write(a.join("meta.json"), r#"{"name":"Alice"}"#).unwrap();
        assert!(e.sw.switch_to("Alice").is_err());
        // ... and live files were not touched by the refused switch? (backup taken, then restore fails -> rolled back)
        assert_eq!(r(&e.sw.paths.desktop.join("config.json")), "cfg-alice");
    }

    #[test]
    fn large_item_alert_exclude_and_keep() {
        let e = env();
        login(&e, "alice");
        // new "massive" thing Anthropic might add
        w(&e.sw.paths.desktop.join("NewBigThing/data.bin"), &"x".repeat(3000));
        w(&e.sw.paths.code_dir.join("plugins/huge.bin"), &"y".repeat(5000));
        assert!(e.sw.scan_large().is_empty()); // default threshold 100 MB
        e.sw.save_settings(&Settings { threshold_mb: 0, manage_code: true, ..Default::default() }).unwrap();
        let l = e.sw.scan_large();
        assert!(l.iter().any(|i| i.root == "desktop" && i.name == "NewBigThing" && i.bytes == 3000));
        assert!(l.iter().any(|i| i.root == "code" && i.name == "plugins"));
        assert!(!l.iter().any(|i| i.name == "vm_bundles" || i.name == "Cache")); // excluded never reported
        assert!(l.iter().any(|i| i.name == "projects")); // session data is part of backups now -> reported when large
        // keep -> no more warning, still included in snapshots
        e.sw.acknowledge_item("desktop", "NewBigThing").unwrap();
        assert!(!e.sw.scan_large().iter().any(|i| i.name == "NewBigThing"));
        e.sw.save_current("A", false).unwrap();
        assert!(e.sw.account_dir("A").join("desktop/NewBigThing/data.bin").exists());
        // exclude -> not warned, not snapshotted
        e.sw.exclude_item("code", "plugins").unwrap();
        assert!(!e.sw.scan_large().iter().any(|i| i.name == "plugins"));
        e.sw.save_current("A", true).unwrap();
        assert!(!e.sw.account_dir("A").join("code/claude/plugins").exists());
    }

    #[test]
    fn new_account_keeps_mcp_and_skills_but_signs_out() {
        let e = env();
        login(&e, "alice");
        e.sw.save_current("Alice", false).unwrap();
        e.sw.begin_new_account().unwrap();
        assert!(!e.sw.desktop_signed_in());
        assert!(!e.sw.paths.code_dir.join(".credentials.json").exists());
        assert!(r(&e.sw.paths.code_json).contains("mcpServers") && !r(&e.sw.paths.code_json).contains("oauthAccount"));
        assert_eq!(r(&e.sw.paths.code_dir.join("skills/s1/SKILL.md")), "skill-alice");
        assert_eq!(r(&e.sw.paths.desktop.join("claude_desktop_config.json")), "mcp-desktop-alice");
    }

    #[test]
    fn manual_backup_while_running_is_lenient() {
        let e = env();
        login(&e, "alice");
        e.sw.save_current("Alice", false).unwrap();
        // strict when closed
        let r1 = e.sw.backup_now().unwrap();
        assert!(!r1.live && r1.skipped.is_empty() && r1.archive.contains("Alice") && r1.archive.ends_with(".tar.gz"));
        // simulate a file we cannot read while Claude runs: a dangling symlink
        #[cfg(unix)]
        std::os::unix::fs::symlink("/nonexistent/locked", e.sw.paths.desktop.join("Local Storage/locked.ldb")).unwrap_or_else(|_| {
            fs::create_dir_all(e.sw.paths.desktop.join("Local Storage")).unwrap();
            std::os::unix::fs::symlink("/nonexistent/locked", e.sw.paths.desktop.join("Local Storage/locked.ldb")).unwrap();
        });
        e.run.store(true, Ordering::SeqCst); // Claude running
        let r2 = e.sw.backup_now().unwrap();
        assert!(r2.live);
        #[cfg(unix)]
        assert!(r2.skipped.iter().any(|x| x.contains("locked.ldb")), "{:?}", r2.skipped);
        // the backup is still restorable (live files untouched, switching still refused while running)
        assert!(e.sw.switch_to("Alice").is_err());
        assert_eq!(e.sw.list_backups().len(), 2);
        // while closed, a bad file makes a strict backup fail instead of silently skipping
        #[cfg(unix)]
        { e.run.store(false, Ordering::SeqCst); assert!(e.sw.backup_now().is_err()); }
    }

    #[test]
    fn reads_desktop_email_from_indexeddb_blob() {
        let e = env();
        let d = e.sw.paths.desktop.join("IndexedDB/https_claude.ai_0.indexeddb.blob/5/01");
        fs::create_dir_all(&d).unwrap();
        let mut blob = b"\xff\x0f..uuid\"$c23444f2\"email_address\"".to_vec();
        blob.push(18);
        blob.extend_from_slice(b"fsadannn@gmail.com");
        blob.extend_from_slice(b"\"full_name\"");
        fs::write(d.join("103"), blob).unwrap();
        assert_eq!(e.sw.live_email().as_deref(), Some("fsadannn@gmail.com"));
        assert_eq!(email_in_blob(b"email_address\"\x03abc"), None);
    }

    #[test]
    fn email_folder_names_are_normalised() {
        assert_eq!(sanitize_email(" Frank.S+test@Gmail.com "), "frank.s_test_at_gmail.com");
        assert_eq!(sanitize_email("a b/c\\d:e*f@x.org"), "a_b_c_d_e_f_at_x.org");
        assert_eq!(sanitize_email("???"), "unknown");
    }

    #[test]
    fn settings_helpers() {
        let e = env();
        e.sw.set_max_backups(Some(0)).unwrap();
        assert_eq!(e.sw.max_backups(), 1);
        e.sw.set_max_backups(None).unwrap();
        assert_eq!(e.sw.max_backups(), DEFAULT_MAX_BACKUPS_PER_ACCOUNT);
        e.sw.set_threshold_mb(7).unwrap();
        e.sw.exclude_item("code", "plugins").unwrap();
        e.sw.acknowledge_item("desktop", "X").unwrap();
        let st = e.sw.settings();
        assert_eq!((st.threshold_mb, st.exclude.len(), st.acknowledged.len()), (7, 1, 1));
        e.sw.remove_exclude("code:plugins").unwrap();
        e.sw.forget_acknowledged("desktop:X").unwrap();
        let st = e.sw.settings();
        assert!(st.exclude.is_empty() && st.acknowledged.is_empty() && st.max_backups.is_none());
        assert_eq!(e.sw.status().settings.threshold_mb, 7);
    }

    #[test]
    fn backups_include_sessions_but_switching_leaves_them_alone() {
        let e = env();
        login(&e, "alice");
        e.sw.save_current("Alice", false).unwrap();
        // not in the account slot
        assert!(!e.sw.account_dir("Alice").join("desktop/local-agent-mode-sessions").exists());
        assert!(!e.sw.account_dir("Alice").join("desktop/claude-code-sessions").exists());
        // but in the backup archive (incl. Claude Code projects/history)
        w(&e.sw.paths.code_dir.join("history.jsonl"), "hist");
        let rep = e.sw.backup_now().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let f = fs::File::open(&rep.archive).unwrap();
        tar::Archive::new(flate2::read::GzDecoder::new(f)).unpack(tmp.path()).unwrap();
        assert_eq!(r(&tmp.path().join("desktop/local-agent-mode-sessions/s1/x.json")), "cowork-alice");
        assert_eq!(r(&tmp.path().join("desktop/claude-code-sessions/s1/y.json")), "ccsess-alice");
        assert_eq!(r(&tmp.path().join("code/claude/projects/p/t.jsonl")), "transcript");
        assert_eq!(r(&tmp.path().join("code/claude/history.jsonl")), "hist");
        assert!(!tmp.path().join("desktop/vm_bundles").exists() && !tmp.path().join("desktop/Cache").exists());
        // switching never touches live sessions
        login(&e, "bob");
        e.sw.save_current("Bob", false).unwrap();
        w(&e.sw.paths.desktop.join("local-agent-mode-sessions/s1/x.json"), "NEWER");
        e.sw.switch_to("Alice").unwrap();
        assert_eq!(r(&e.sw.paths.desktop.join("local-agent-mode-sessions/s1/x.json")), "NEWER");
        // explicit backup restore brings sessions back
        e.sw.restore_backup(Path::new(&rep.archive)).unwrap();
        assert_eq!(r(&e.sw.paths.desktop.join("local-agent-mode-sessions/s1/x.json")), "cowork-alice");
        assert_eq!(r(&e.sw.paths.code_dir.join("history.jsonl")), "hist");
    }

    #[test]
    fn git_toggle_and_full_restore() {
        let e = env();
        login(&e, "alice");
        let p = e.sw.paths.clone();
        w(&p.desktop.join("git-shadow/repo/obj"), "G1");
        w(&p.desktop.join("git-worktrees.json"), "W1");
        w(&p.desktop.join("scratch-workspaces/a/f"), "S1");
        e.sw.save_current("Alice", false).unwrap();
        // toggle off (default): git data not in backups, not in slots
        assert!(!e.sw.settings().backup_git);
        let r1 = e.sw.backup_now().unwrap();
        let unpack = |arch: &str| { let t = tempfile::tempdir().unwrap();
            tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(arch).unwrap())).unpack(t.path()).unwrap(); t };
        let t1 = unpack(&r1.archive);
        assert!(!t1.path().join("desktop/git-shadow").exists() && !t1.path().join("desktop/scratch-workspaces").exists());
        assert!(!t1.path().join("desktop/git-worktrees.json").exists());
        assert!(!e.sw.scan_large().iter().any(|i| i.name == "git-shadow"));
        // toggle on: included
        e.sw.set_backup_git(true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        let r2 = e.sw.backup_now().unwrap();
        let t2 = unpack(&r2.archive);
        assert_eq!(r(&t2.path().join("desktop/git-shadow/repo/obj")), "G1");
        assert_eq!(r(&t2.path().join("desktop/git-worktrees.json")), "W1");
        assert_eq!(r(&t2.path().join("desktop/scratch-workspaces/a/f")), "S1");
        // never part of a switch slot, even when on
        assert!(!e.sw.account_dir("Alice").join("desktop/git-shadow").exists());

        // --- full restore = everything inside the archive, regardless of today's settings
        e.sw.set_backup_git(false).unwrap();
        e.sw.exclude_item("desktop", "local-agent-mode-sessions").unwrap(); // excluded AFTER the backup was taken? (r2 was before this)
        w(&p.desktop.join("local-agent-mode-sessions/s1/x.json"), "CHANGED");
        w(&p.desktop.join("git-shadow/repo/obj"), "G-CHANGED");
        w(&p.desktop.join("brand-new-live-only"), "keep me"); // not in archive -> must survive
        e.sw.restore_backup(Path::new(&r2.archive)).unwrap();
        assert_eq!(r(&p.desktop.join("local-agent-mode-sessions/s1/x.json")), "cowork-alice"); // restored despite exclude
        assert_eq!(r(&p.desktop.join("git-shadow/repo/obj")), "G1"); // restored although toggle is off now
        assert_eq!(r(&p.desktop.join("scratch-workspaces/a/f")), "S1");
        assert_eq!(r(&p.desktop.join("brand-new-live-only")), "keep me");
        assert_eq!(r(&p.desktop.join("config.json")), "cfg-alice");
        // restoring an old backup (no git) must not delete live git data
        w(&p.desktop.join("git-shadow/repo/obj"), "G-LIVE");
        e.sw.restore_backup(Path::new(&r1.archive)).unwrap();
        assert_eq!(r(&p.desktop.join("git-shadow/repo/obj")), "G-LIVE");
    }

    #[test]
    fn targets_desktop_only_and_code_only() {
        // Desktop only: ~/.claude + .claude.json are neither saved nor touched
        let e = env();
        login(&e, "alice");
        e.sw.set_targets(true, false).unwrap();
        e.sw.save_current("Alice", false).unwrap();
        assert!(e.sw.account_dir("Alice").join("desktop/config.json").exists());
        assert!(!e.sw.account_dir("Alice").join("code/claude").join("skills").exists());
        let bk = e.sw.backup_now().unwrap();
        assert!(!bk.archive.is_empty());
        login(&e, "bob");
        e.sw.save_current("Bob", false).unwrap();
        e.sw.switch_to("Alice").unwrap();
        assert_eq!(r(&e.sw.paths.desktop.join("config.json")), "cfg-alice");
        assert_eq!(r(&e.sw.paths.code_dir.join(".credentials.json")), "cred-bob"); // untouched
        assert!(r(&e.sw.paths.code_json).contains("bob@x.com"));
        // switching to a slot saved with Code, while Code is off, still leaves ~/.claude alone
        e.sw.set_targets(true, true).unwrap();
        e.sw.save_current("Both", false).unwrap();
        e.sw.set_targets(true, false).unwrap();
        e.sw.switch_to("Both").unwrap();

        // Code only: Desktop dir is neither saved nor touched
        let e = env();
        login(&e, "alice");
        e.sw.set_targets(false, true).unwrap();
        e.sw.save_current("Alice", false).unwrap();
        assert!(!e.sw.account_dir("Alice").join("desktop/config.json").exists());
        assert_eq!(r(&e.sw.account_dir("Alice").join("code/claude/.credentials.json")), "cred-alice");
        login(&e, "bob");
        e.sw.save_current("Bob", false).unwrap();
        e.sw.switch_to("Alice").unwrap();
        assert_eq!(r(&e.sw.paths.code_dir.join(".credentials.json")), "cred-alice");
        assert_eq!(r(&e.sw.paths.desktop.join("config.json")), "cfg-bob"); // untouched
        // explicit restore of an archive restores the parts that archive has, whatever the toggles
        assert!(e.sw.set_targets(false, false).is_err());
    }

    #[test]
    fn default_is_desktop_only_and_never_touches_claude_code_files() {
        let t = tempfile::tempdir().unwrap();
        let b = t.path();
        let paths = Paths { desktop: b.join("desk"), code_dir: b.join("home/.claude"), code_json: b.join("home/.claude.json"), store: b.join("store") };
        let sw = Swapper::new(paths.clone(), Box::new(|| false)); // NOTE: default settings, no set_targets
        assert!(sw.settings().manage_desktop && !sw.settings().manage_code);
        let e = Env { _t: tempfile::tempdir().unwrap(), sw, run: Arc::new(AtomicBool::new(false)) };
        login(&e, "alice");
        let before = (r(&paths.code_json), r(&paths.code_dir.join(".credentials.json")), r(&paths.code_dir.join("skills/s1/SKILL.md")));
        e.sw.save_current("Alice", false).unwrap();
        login(&e, "bob");
        let bob_code = (r(&paths.code_json), r(&paths.code_dir.join(".credentials.json")));
        e.sw.save_current("Bob", false).unwrap();
        e.sw.switch_to("Alice").unwrap();
        e.sw.begin_new_account().unwrap();
        let rep = e.sw.backup_now().unwrap();
        e.sw.recover().unwrap();
        // Desktop side switched …
        assert_eq!(r(&paths.desktop.join("config.json")), "cfg-alice");
        // … Claude Code side byte-identical to what login(bob) left there
        assert_eq!((r(&paths.code_json), r(&paths.code_dir.join(".credentials.json"))), bob_code);
        assert_ne!(before.1, bob_code.1);
        assert!(r(&paths.code_dir.join("skills/s1/SKILL.md")).contains("bob"));
        // nothing from Claude Code in slots or archives, no e-mail read from ~/.claude.json (it comes from Desktop's own files)
        assert!(!e.sw.account_dir("Alice").join("code/claude/skills").exists());
        let tmp = tempfile::tempdir().unwrap();
        tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(&rep.archive).unwrap())).unpack(tmp.path()).unwrap();
        assert!(!tmp.path().join("code/claude/.credentials.json").exists() && !tmp.path().join("code/claude/skills").exists());
        assert!(rep.archive.contains("_unassigned") && !rep.archive.contains("@"));
        assert!(e.sw.status().live_email.is_none());
    }

    #[test]
    fn refuses_when_running() {
        let e = env();
        login(&e, "alice");
        e.run.store(true, Ordering::SeqCst);
        assert!(e.sw.save_current("A", false).is_err());
    }

    #[test]
    fn recover_after_cancel() {
        let e = env();
        login(&e, "alice");
        e.sw.save_current("Alice", false).unwrap();
        e.sw.begin_new_account().unwrap();
        assert!(e.sw.status().pending_recovery);
        e.sw.recover().unwrap();
        assert_eq!(r(&e.sw.paths.desktop.join("config.json")), "cfg-alice");
        assert_eq!(e.sw.active().as_deref(), Some("Alice"));
        assert!(!e.sw.status().pending_recovery);
    }

    #[test]
    fn names_and_exe() {
        assert!(validate_name("../x").is_err());
        assert!(validate_name("Work 2").is_ok());
        assert!(is_desktop_exe(Path::new(r"C:\Program Files\WindowsApps\Claude_1.0_x64__abc\app\Claude.exe")));
        assert!(!is_desktop_exe(Path::new(r"C:\Users\f\.local\bin\claude.exe")));
    }
}
