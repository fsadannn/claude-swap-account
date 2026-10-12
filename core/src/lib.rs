//! swap-core: back up and switch the account state of **Claude Desktop** (MSIX app on Windows) and of the
//! **Claude Code CLI** (`~/.claude`, `~/.claude.json`), one account active at a time *per instance*.
//! No GUI, no async – the `claude-swap` app is a thin layer on top.
//!
//! Desktop and the CLI can run at the same time and are signed in independently, so everything is tagged with
//! an [`Instance`]: account slots, the active account, the recovery marker, backups (in the archive's
//! `meta.json` and in the file name) and the running check. Operating on one instance never touches the other.
//!
//! Layout of the store (`%LOCALAPPDATA%\claude-swap`):
//! ```text
//! accounts/desktop/<name>/desktop/<entries...>    snapshot of the Desktop data folder (switch scope)
//! accounts/code/<name>/code/claude/<entries...>   snapshot of ~/.claude (+ code/claude-json.json: swapped keys of ~/.claude.json)
//! accounts/<instance>/<name>/meta.json            name, e-mail, format, instance
//! backups/<account>/<ms>-<instance>-<reason>.tar.gz  backup of the live state taken before any overwrite and on demand
//!                                         (backup scope = switch scope + session data);
//!                                         <account> = active account of that instance, else "<email>_unassigned" / "_unassigned";
//!                                         newest N kept per folder AND instance (see `Swapper::max_backups`)
//! settings.json                           user settings (`Settings`)
//! state.json                              { "active": { "desktop": "<name>", "code": "<name>" } }
//! pending.json / pending-code.json        crash-recovery marker per instance (live state may be half-written)
//! ```
//! Older stores are migrated on start: `accounts/<name>` -> `accounts/desktop/<name>`, `state.json` with a single
//! `"active": "<name>"` is read as the Desktop entry, backups without an instance in the name are Desktop backups.
//!
//! Two scopes (`Scope`): **Switch** = what makes up an account (sign-in, MCP config, extensions, skills);
//! **Backup** = Switch + session data (+ git data if enabled, + CLI transcripts if `backup_code_sessions`).
//! Both are "whole folder minus excludes", so files Anthropic adds later are picked up automatically;
//! `Swapper::scan_large` warns when something big shows up.
//! Restoring a backup puts back everything inside the archive and never deletes live data that is not in it.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Snapshot format. v3 = one instance per snapshot/archive (tagged in `meta.json`); v2 = "whole folder minus
/// excludes" without the tag (always Desktop, still restorable); v1 was a short allow-list and is not restorable.
pub const FORMAT: u32 = 3;
const MIN_FORMAT: u32 = 2;

/// Which Claude installation an operation is about. They run side by side and are signed in independently.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum Instance {
    /// Claude Desktop (its data folder)
    #[default]
    Desktop,
    /// Claude Code CLI (`~/.claude` + `~/.claude.json`; also what IDE extensions use)
    Code,
}

impl Instance {
    pub const ALL: [Instance; 2] = [Instance::Desktop, Instance::Code];

    /// folder / file-name / settings-key form
    pub fn key(self) -> &'static str {
        match self { Instance::Desktop => "desktop", Instance::Code => "code" }
    }

    /// what the UI calls it
    pub fn label(self) -> &'static str {
        match self { Instance::Desktop => "Desktop", Instance::Code => "CLI" }
    }

    pub fn from_key(s: &str) -> Option<Instance> {
        Instance::ALL.into_iter().find(|i| i.key() == s)
    }

    /// folder inside a snapshot / archive that holds this instance's files
    fn part_dir(self) -> &'static str {
        match self { Instance::Desktop => "desktop", Instance::Code => "code/claude" }
    }
}

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
/// Claude Code: top-level entries of `~/.claude` that are NOT swapped
/// (session transcripts, caches, history). Everything else (credentials, settings.json, skills/, commands/,
/// agents/, plugins/, CLAUDE.md, ...) is swapped.
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
/// CLI session data (transcripts, todos, plans, prompt history): in backups unless
/// `Settings::backup_code_sessions` is turned off (never part of a switch).
pub const CODE_SESSION_ITEMS: &[&str] = &["projects", "todos", "plans", "history.jsonl"];

/// What a snapshot/restore covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// account switching: sign-in, MCP, skills, settings (sessions are left alone)
    Switch,
    /// backups: everything in `Switch` plus session data
    Backup,
}

/// Keys of `~/.claude.json` that are swapped (the rest – project history etc. – is left alone).
/// `mcpServers` = user-scope MCP servers of Claude Code.
pub const CODE_JSON_KEYS: &[&str] = &["oauthAccount", "mcpServers"];

/// Never copied at any depth (Chromium lock/singleton files).
const SKIP_FILES: &[&str] = &[
    "SingletonLock", "SingletonCookie", "SingletonSocket", "lockfile", "LOCK",
];
/// Default number of backup archives kept per account folder and instance
/// (override with `Swapper::with_max_backups(Some(n))` or `max_backups` in settings.json).
pub const DEFAULT_MAX_BACKUPS_PER_ACCOUNT: usize = 15;
const CODE_CREDS: &str = ".credentials.json";
const OAUTH_KEY: &str = "oauthAccount";
const DEFAULT_THRESHOLD_MB: u64 = 100;

#[derive(Clone, Debug)]
pub struct Paths {
    /// Claude Desktop user-data dir (MSIX virtualised Roaming\Claude, or %APPDATA%\Claude).
    pub desktop: PathBuf,
    /// ~/.claude
    pub code_dir: PathBuf,
    /// ~/.claude.json
    pub code_json: PathBuf,
    /// our store
    pub store: PathBuf,
}

impl Paths {
    /// Auto-detect on the current machine (Windows). Claude Desktop does not have to be installed:
    /// without it the Desktop paths just point at the default location and the CLI still works.
    pub fn detect() -> Result<Paths> {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("no home dir"))?;
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        let roaming = std::env::var_os("APPDATA").map(PathBuf::from);
        let desktop = find_desktop_dir(local.as_deref(), roaming.as_deref())
            .or_else(|| roaming.as_ref().map(|r| r.join("Claude")))
            .unwrap_or_else(|| home.join("AppData").join("Roaming").join("Claude"));
        let store = local.unwrap_or_else(|| home.clone()).join("claude-swap");
        Ok(Paths {
            desktop,
            code_dir: home.join(".claude"),
            code_json: home.join(".claude.json"),
            store,
        })
    }

    /// The live data folder of an instance.
    pub fn dir(&self, i: Instance) -> &Path {
        match i { Instance::Desktop => &self.desktop, Instance::Code => &self.code_dir }
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
    /// which Claude this snapshot is of (missing in v2 snapshots = Desktop)
    #[serde(default)]
    pub instance: Instance,
}

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
    /// backups kept per account folder and instance; `None` = built-in default
    pub max_backups: Option<usize>,
    /// also put Desktop's git data (`DESKTOP_GIT_ITEMS`, can be large) into backups
    pub backup_git: bool,
    /// also put the CLI's session data (`CODE_SESSION_ITEMS`: transcripts, todos, plans, history) into backups
    pub backup_code_sessions: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Settings { threshold_mb: DEFAULT_THRESHOLD_MB, exclude: vec![], acknowledged: vec![], max_backups: None, backup_git: false, backup_code_sessions: true }
    }
}

/// Result of a manual backup.
#[derive(Serialize, Clone, Debug)]
pub struct BackupReport {
    pub instance: Instance,
    pub archive: String,
    /// files that could not be read (locked by a running Claude)
    pub skipped: Vec<String>,
    /// true if that Claude was running (best-effort backup)
    pub live: bool,
    pub size_bytes: u64,
}

/// One backup archive on disk.
#[derive(Serialize, Clone, Debug)]
pub struct BackupEntry {
    /// account folder the archive lives in
    pub owner: String,
    pub instance: Instance,
    pub path: PathBuf,
}

/// A big top-level item that would be included in every backup / account snapshot.
#[derive(Serialize, Clone, Debug)]
pub struct LargeItem {
    /// "desktop" or "code"
    pub root: String,
    pub name: String,
    pub bytes: u64,
}

/// Everything the UI shows about one instance.
#[derive(Serialize, Clone, Debug)]
pub struct InstanceStatus {
    pub instance: Instance,
    /// the live data folder
    pub dir: String,
    pub running: bool,
    /// e-mail of the signed-in account; `None` if not found
    pub live_email: Option<String>,
    pub signed_in: bool,
    pub active: Option<String>,
    pub pending_recovery: bool,
    pub accounts: Vec<Meta>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Status {
    pub store_dir: String,
    pub backups_dir: String,
    pub desktop: InstanceStatus,
    pub code: InstanceStatus,
    pub settings: Settings,
    /// effective backup limit per account and instance (after parameter/settings/default resolution)
    pub max_backups: usize,
}

impl Status {
    pub fn get(&self, i: Instance) -> &InstanceStatus {
        match i { Instance::Desktop => &self.desktop, Instance::Code => &self.code }
    }
}

/// Is that instance running? (injected so tests don't need processes)
pub type RunningFn = Box<dyn Fn(Instance) -> bool + Send + Sync>;

pub struct Swapper {
    pub paths: Paths,
    pub is_running: RunningFn,
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

/// Instance of a backup archive, from its file name (`<ms>-<instance>-<reason>.tar.gz`).
/// Archives from before the tag existed (`<ms>-<reason>.tar.gz`) are Desktop backups.
fn backup_instance(p: &Path) -> Instance {
    p.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.split('-').nth(1))
        .and_then(Instance::from_key)
        .unwrap_or(Instance::Desktop)
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

fn read_meta(dir: &Path) -> Meta {
    fs::read_to_string(dir.join("meta.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

// ---------------------------------------------------------------- impl

impl Swapper {
    pub fn new(paths: Paths, is_running: RunningFn) -> Self {
        let s = Swapper { paths, is_running, max_backups: None };
        s.migrate_layout();
        s
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

    fn backups_dir(&self) -> PathBuf { self.paths.store.join("backups") }
    fn state_file(&self) -> PathBuf { self.paths.store.join("state.json") }
    /// `pending.json` stays the Desktop marker so a marker written by an older version is still found.
    fn pending_file(&self, i: Instance) -> PathBuf {
        self.paths.store.join(match i { Instance::Desktop => "pending.json", Instance::Code => "pending-code.json" })
    }
    fn accounts_dir(&self, i: Instance) -> PathBuf { self.paths.store.join("accounts").join(i.key()) }
    fn account_dir(&self, i: Instance, n: &str) -> PathBuf { self.accounts_dir(i).join(n) }

    /// Old stores kept Desktop slots directly in `accounts/<name>`; they now live in `accounts/desktop/<name>`.
    /// Old slots are told apart by a `meta.json` right inside them (instance folders have none). Each one is
    /// staged as `.mig-<name>` first because an old account may itself be called "desktop" or "code".
    /// Best-effort and idempotent: whatever fails is retried on the next start.
    fn migrate_layout(&self) {
        let root = self.paths.store.join("accounts");
        let Ok(rd) = fs::read_dir(&root) else { return };
        let mut staged: Vec<(String, PathBuf)> = vec![];
        for e in rd.flatten() {
            let p = e.path();
            if !p.join("meta.json").is_file() { continue; }
            let n = e.file_name().to_string_lossy().to_string();
            match n.strip_prefix(".mig-") {
                Some(real) => staged.push((real.to_string(), p)),
                None => {
                    let stage = root.join(format!(".mig-{n}"));
                    if fs::rename(&p, &stage).is_ok() { staged.push((n, stage)); }
                }
            }
        }
        for (n, stage) in staged {
            let dst = self.account_dir(Instance::Desktop, &n);
            if dst.exists() { continue; }
            if fs::create_dir_all(self.accounts_dir(Instance::Desktop)).is_ok() {
                let _ = fs::rename(&stage, &dst);
            }
        }
    }

    fn ensure_closed(&self, i: Instance) -> Result<()> {
        if (self.is_running)(i) {
            match i {
                Instance::Desktop => bail!("Claude Desktop is running – close it first (or use 'Close Claude')."),
                Instance::Code => bail!("Claude Code (CLI) is running – close all its sessions (terminals, IDE extensions) first."),
            }
        }
        Ok(())
    }

    /// Active account of an instance. `state.json` used to hold a single `"active": "<name>"` (Desktop).
    pub fn active(&self, i: Instance) -> Option<String> {
        let m = read_json_obj(&self.state_file())?;
        match m.get("active")? {
            Value::String(s) => (i == Instance::Desktop).then(|| s.clone()),
            Value::Object(o) => o.get(i.key())?.as_str().map(String::from),
            _ => None,
        }
    }

    /// The saved account that is REALLY live, as opposed to `active` (what we last switched to).
    /// They differ after a manual login / `/login` outside this tool. The signed-in e-mail decides:
    ///  * it matches a saved slot -> that slot,
    ///  * it matches none -> `None` (an unsaved account: never write it into another account's slot),
    ///  * e-mail unknown -> fall back to `active`.
    fn current(&self, i: Instance) -> Option<String> {
        let stored = self.active(i);
        let Some(live) = self.live_email(i) else { return stored };
        let same = |m: &Meta| m.email.as_deref().map_or(false, |e| e.eq_ignore_ascii_case(&live));
        let slots = self.list(i);
        if let Some(a) = &stored {
            if slots.iter().any(|m| &m.name == a && (same(m) || m.email.is_none())) { return stored; }
        }
        slots.into_iter().find(|m| same(m)).map(|m| m.name)
    }

    fn set_active(&self, i: Instance, a: Option<&str>) -> Result<()> {
        fs::create_dir_all(&self.paths.store)?;
        let mut cur = match read_json_obj(&self.state_file()).and_then(|m| m.get("active").cloned()) {
            Some(Value::Object(o)) => o,
            Some(Value::String(s)) => { let mut o = Map::new(); o.insert("desktop".into(), s.into()); o }
            _ => Map::new(),
        };
        match a {
            Some(n) => { cur.insert(i.key().into(), n.into()); }
            None => { cur.remove(i.key()); }
        }
        let v = serde_json::json!({ "active": cur });
        write_atomic(&self.state_file(), serde_json::to_string_pretty(&v)?.as_bytes())
    }

    /// E-mail of whoever is signed in. Claude Desktop keeps it (plain text, V8-serialised) in the
    /// claude.ai IndexedDB blobs; Claude Code keeps it in `~/.claude.json` (`oauthAccount`).
    fn live_email(&self, i: Instance) -> Option<String> {
        match i {
            Instance::Desktop => desktop_email(&self.paths.desktop),
            Instance::Code => read_json_obj(&self.paths.code_json)?
                .get(OAUTH_KEY)?
                .get("emailAddress")?
                .as_str()
                .map(String::from),
        }
    }

    /// Desktop: sign-in files exist. CLI: credentials file exists.
    fn signed_in(&self, i: Instance) -> bool {
        match i {
            Instance::Desktop => REQUIRED.iter().all(|r| self.paths.desktop.join(r).exists()),
            Instance::Code => self.paths.code_dir.join(CODE_CREDS).exists(),
        }
    }

    pub fn list(&self, i: Instance) -> Vec<Meta> {
        let mut v: Vec<Meta> = fs::read_dir(self.accounts_dir(i))
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
                Some(m.unwrap_or(Meta { name, instance: i, ..Default::default() }))
            })
            .collect();
        v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        v
    }

    fn instance_status(&self, i: Instance) -> InstanceStatus {
        InstanceStatus {
            instance: i,
            dir: self.paths.dir(i).display().to_string(),
            running: (self.is_running)(i),
            live_email: self.live_email(i),
            signed_in: self.signed_in(i),
            active: self.current(i),
            pending_recovery: self.pending_file(i).exists(),
            accounts: self.list(i),
        }
    }

    pub fn status(&self) -> Status {
        Status {
            store_dir: self.paths.store.display().to_string(),
            backups_dir: self.backups_dir().display().to_string(),
            desktop: self.instance_status(Instance::Desktop),
            code: self.instance_status(Instance::Code),
            settings: self.settings(),
            max_backups: self.max_backups(),
        }
    }

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
        let sessions_off = scope == Scope::Backup
            && root == "code"
            && !st.backup_code_sessions
            && CODE_SESSION_ITEMS.iter().any(|e| e.eq_ignore_ascii_case(name));
        git_off
            || sessions_off
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
        for i in Instance::ALL {
            let (root, dir) = (i.key(), self.paths.dir(i));
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

    pub fn set_backup_git(&self, on: bool) -> Result<()> {
        let mut st = self.settings();
        st.backup_git = on;
        self.save_settings(&st)
    }

    pub fn set_backup_code_sessions(&self, on: bool) -> Result<()> {
        let mut st = self.settings();
        st.backup_code_sessions = on;
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

    fn snapshot_to(&self, dir: &Path, name: &str, i: Instance) -> Result<()> {
        self.snapshot_impl(dir, name, i, Scope::Switch, false, &mut vec![])
    }

    /// Copy the live files of ONE instance into `dir`, plus a `meta.json` tagged with that instance.
    fn snapshot_impl(&self, dir: &Path, name: &str, i: Instance, scope: Scope, lenient: bool, skipped: &mut Vec<String>) -> Result<()> {
        let st = self.settings();
        let part = dir.join(i.part_dir());
        fs::create_dir_all(&part)?;
        let live = self.paths.dir(i);
        for n in self.managed(i.key(), live, &st, scope) {
            copy_tree(&live.join(&n), &part.join(&n), lenient, skipped)?;
        }
        if i == Instance::Code {
            let mut keys = Map::new();
            if let Some(m) = read_json_obj(&self.paths.code_json) {
                for k in CODE_JSON_KEYS {
                    if let Some(v) = m.get(*k) { keys.insert((*k).into(), v.clone()); }
                }
            }
            fs::write(dir.join("code").join("claude-json.json"), serde_json::to_string_pretty(&keys)?)?;
        }
        let meta = Meta { name: name.to_string(), email: self.live_email(i), saved_at_ms: now_ms(), format: FORMAT, instance: i };
        fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
        Ok(())
    }

    /// Put a snapshot / unpacked archive back. The instance comes from its `meta.json`.
    fn restore_from(&self, dir: &Path, scope: Scope) -> Result<()> {
        let meta = read_meta(dir);
        if meta.format < MIN_FORMAT {
            bail!("this snapshot was saved by an older version – sign in to that account and save it again");
        }
        let i = meta.instance;
        let st = self.settings();
        // What gets removed from the live folder / copied from the snapshot:
        //  * Switch: the managed (account) entries only.
        //  * Backup (explicit restore): EVERYTHING that is inside the archive, whatever today's exclude
        //    settings are. Each such entry is replaced as a whole; live entries that are not in the
        //    archive are never deleted.
        let full = scope == Scope::Backup;
        let snap = dir.join(i.part_dir());
        if !snap.is_dir() { bail!("snapshot has no {} data", i.label()); }
        let live = self.paths.dir(i);
        fs::create_dir_all(live)?;
        let copy: Vec<String> = if full { list_entries(&snap) } else { self.managed(i.key(), &snap, &st, Scope::Switch) };
        let remove = if full { copy.clone() } else { self.managed(i.key(), live, &st, Scope::Switch) };
        for n in remove { remove_any(&live.join(&n))?; }
        for n in copy { copy_recursive(&snap.join(&n), &live.join(&n))?; }
        if i == Instance::Code {
            // ~/.claude.json: merge only the swapped keys, keep everything else
            let saved: Map<String, Value> = read_json_obj(&dir.join("code").join("claude-json.json")).unwrap_or_default();
            self.merge_code_json(&saved)?;
        }
        Ok(())
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

    /// Sign out of one instance but keep MCP config / extensions / settings in place so a newly added
    /// account starts from the current setup.
    fn sign_out_live(&self, i: Instance) -> Result<()> {
        match i {
            Instance::Desktop => {
                for it in DESKTOP_IDENTITY { remove_any(&self.paths.desktop.join(it))?; }
            }
            Instance::Code => {
                remove_any(&self.paths.code_dir.join(CODE_CREDS))?;
                if let Some(mut m) = read_json_obj(&self.paths.code_json) {
                    if m.remove(OAUTH_KEY).is_some() { self.write_code_json(&m)?; }
                }
            }
        }
        Ok(())
    }

    fn write_code_json(&self, m: &Map<String, Value>) -> Result<()> {
        write_atomic(&self.paths.code_json, serde_json::to_string_pretty(m)?.as_bytes())
    }

    /// Back up the live state of an instance (always before overwriting it) with `Scope::Backup`, as
    /// `backups/<account>/<millis>-<instance>-<reason>.tar.gz` where `<account>` is that instance's active
    /// account, else `<email>_unassigned` (or `_unassigned`). Prunes old archives. Returns the archive path.
    fn backup_live(&self, i: Instance, reason: &str) -> Result<PathBuf> {
        Ok(self.backup_impl(i, reason, false)?.0)
    }

    fn backup_impl(&self, i: Instance, reason: &str, lenient: bool) -> Result<(PathBuf, Vec<String>)> {
        let mut skipped: Vec<String> = vec![];
        // Folder = active account's name. With no saved account active the backup is still labelled by
        // whoever is signed in: "<email>_unassigned" (or plain "_unassigned" if that is unknown).
        let owner = self.current(i).unwrap_or_else(|| match self.live_email(i) {
            Some(m) => format!("{}_unassigned", sanitize_email(&m)),
            None => "_unassigned".to_string(),
        });
        let folder = self.backups_dir().join(&owner);
        fs::create_dir_all(&folder)?;
        let stem = format!("{}-{}-{}", now_ms(), i.key(), sanitize(reason));
        let tmp = self.paths.store.join(format!(".tmp-bk-{stem}"));
        remove_any(&tmp)?;
        let archive = folder.join(format!("{stem}.tar.gz"));
        let res = (|| -> Result<()> {
            self.snapshot_impl(&tmp, "backup", i, Scope::Backup, lenient, &mut skipped)?;
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
        self.prune_backups(&folder, i);
        Ok((archive, skipped))
    }

    /// Keep the newest N archives of this instance in `folder` (the other instance's archives are not counted).
    fn prune_backups(&self, folder: &Path, i: Instance) {
        let mut v: Vec<PathBuf> = fs::read_dir(folder)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".tar.gz") && backup_instance(p) == i)
            .collect();
        v.sort(); // names start with epoch millis -> oldest first
        let max = self.max_backups();
        while v.len() > max {
            let _ = fs::remove_file(v.remove(0));
        }
    }

    /// Manual backup of the CURRENT state of one instance. Works while that Claude is running: files it holds
    /// locked are skipped (listed in the report) and SQLite/LevelDB files may be mid-write, so a
    /// live backup is best-effort. With it closed it is an exact, strict backup.
    pub fn backup_now(&self, i: Instance) -> Result<BackupReport> {
        let live = (self.is_running)(i);
        let (archive, skipped) = self.backup_impl(i, if live { "manual-live" } else { "manual" }, live)?;
        let size_bytes = fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
        Ok(BackupReport { instance: i, archive: archive.display().to_string(), skipped, live, size_bytes })
    }

    /// All backup archives (both instances), newest first.
    pub fn list_backups(&self) -> Vec<BackupEntry> {
        let mut out = vec![];
        for d in fs::read_dir(self.backups_dir()).into_iter().flatten().flatten() {
            if !d.path().is_dir() { continue; }
            let owner = d.file_name().to_string_lossy().to_string();
            for f in fs::read_dir(d.path()).into_iter().flatten().flatten() {
                let path = f.path();
                if path.to_string_lossy().ends_with(".tar.gz") {
                    out.push(BackupEntry { owner: owner.clone(), instance: backup_instance(&path), path });
                }
            }
        }
        out.sort_by(|a, b| b.path.file_name().cmp(&a.path.file_name()));
        out
    }

    /// Restore a backup archive (the Claude it is a backup of must be closed): puts back EVERYTHING inside
    /// it, whatever the current exclude / git / session settings are, and never deletes live entries that are
    /// not in the archive. The instance is the one the archive was taken from.
    pub fn restore_backup(&self, archive: &Path) -> Result<()> {
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
            let i = read_meta(&tmp).instance;
            if !tmp.join(i.part_dir()).is_dir() { bail!("not a valid backup: {}", archive.display()); }
            if scope == Scope::Backup { self.ensure_closed(i)?; }
            self.restore_from(&tmp, scope)
        })();
        let _ = remove_any(&tmp);
        res
    }

    fn mark_pending(&self, i: Instance, backup: &Path) -> Result<()> {
        fs::create_dir_all(&self.paths.store)?;
        let v = serde_json::json!({ "backup": backup.display().to_string(), "instance": i });
        write_atomic(&self.pending_file(i), v.to_string().as_bytes())
    }
    fn clear_pending(&self, i: Instance) { let _ = fs::remove_file(self.pending_file(i)); }

    /// Save the live state of `i` into slot `name` (build in temp dir, then swap in).
    fn save_slot(&self, i: Instance, name: &str) -> Result<()> {
        let accounts = self.accounts_dir(i);
        let tmp = accounts.join(format!(".tmp-{}", now_ms()));
        fs::create_dir_all(&tmp)?;
        let r = self.snapshot_to(&tmp, name, i);
        if let Err(e) = r { let _ = fs::remove_dir_all(&tmp); return Err(e); }
        let dst = self.account_dir(i, name);
        remove_any(&dst)?;
        fs::rename(&tmp, &dst)?;
        Ok(())
    }

    // ---- public operations

    /// "Save current login as <name>". Use after signing in to an account.
    pub fn save_current(&self, i: Instance, name: &str, overwrite: bool) -> Result<()> {
        validate_name(name)?;
        self.ensure_closed(i)?;
        if !self.signed_in(i) {
            match i {
                Instance::Desktop => bail!("Claude Desktop does not look signed in (config.json / Local State missing)."),
                Instance::Code => bail!("Claude Code does not look signed in (~/.claude/.credentials.json missing)."),
            }
        }
        if self.account_dir(i, name).exists() && !overwrite {
            bail!("a {} account named '{name}' already exists", i.label());
        }
        // Never write a different account's login over a saved slot (e.g. after a manual /login):
        // the slot would silently turn into that other account.
        if let (Some(live), Some(slot)) = (self.live_email(i), self.list(i).into_iter().find(|m| m.name == name)) {
            if let Some(se) = slot.email.as_deref().filter(|se| !se.eq_ignore_ascii_case(&live)) {
                bail!(
                    "'{name}' holds {se}, but {live} is signed in – not overwriting it. \
                     Save under a new name, or delete '{name}' first if you really mean to replace it."
                );
            }
        }
        self.save_slot(i, name)?;
        self.set_active(i, Some(name))
    }

    /// Switch `i` to saved account `name`: saves the current one back to its slot first
    /// (so refreshed tokens are kept), backs up live files, then restores.
    pub fn switch_to(&self, i: Instance, name: &str) -> Result<()> {
        validate_name(name)?;
        self.ensure_closed(i)?;
        let target = self.account_dir(i, name);
        if !target.join(i.part_dir()).is_dir() {
            bail!("no saved {} account named '{name}'", i.label());
        }
        if let Some(a) = self.current(i) {
            if a != name && self.signed_in(i) && self.account_dir(i, &a).exists() {
                self.save_slot(i, &a)?;
            }
        }
        let backup = self.backup_live(i, &format!("switch-to-{name}"))?;
        self.mark_pending(i, &backup)?;
        match self.restore_from(&target, Scope::Switch) {
            Ok(()) => { self.clear_pending(i); self.set_active(i, Some(name)) }
            Err(e) => {
                // roll back from the backup we just took
                let rb = self.restore_from_archive(&backup, Scope::Switch);
                if rb.is_ok() { self.clear_pending(i); }
                Err(e.context(if rb.is_ok() { "switch failed, previous state restored" } else { "switch failed AND rollback failed – use Recover" }))
            }
        }
    }

    /// Prepare to log in to a NEW account: stores the current account (if active),
    /// backs up and clears the sign-in files. Then sign in and call `save_current`.
    pub fn begin_new_account(&self, i: Instance) -> Result<()> {
        self.ensure_closed(i)?;
        if let Some(a) = self.current(i) {
            if self.signed_in(i) && self.account_dir(i, &a).exists() {
                self.save_slot(i, &a)?;
            }
        }
        let backup = self.backup_live(i, "before-new-account")?;
        self.mark_pending(i, &backup)?;
        self.sign_out_live(i)?;
        self.set_active(i, None)?;
        // pending stays until save_current() or recover() so a crash/cancel can be undone
        Ok(())
    }

    /// Call after a successful `save_current` following `begin_new_account`.
    pub fn finish_new_account(&self, i: Instance) { self.clear_pending(i); }

    /// Undo: restore the live state of `i` from the backup referenced by its pending marker.
    pub fn recover(&self, i: Instance) -> Result<String> {
        self.ensure_closed(i)?;
        let p = read_json_obj(&self.pending_file(i)).ok_or_else(|| anyhow!("nothing to recover"))?;
        let b = PathBuf::from(p.get("backup").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("bad marker"))?);
        if !b.is_file() { bail!("backup missing: {}", b.display()); }
        self.restore_from_archive(&b, Scope::Switch)?;
        self.clear_pending(i);
        // active = the account whose email matches, if we can tell
        let email = self.live_email(i);
        let act = self.list(i).into_iter().find(|m| m.email.is_some() && m.email == email).map(|m| m.name);
        self.set_active(i, act.as_deref())?;
        Ok(b.display().to_string())
    }

    pub fn delete_account(&self, i: Instance, name: &str) -> Result<()> {
        validate_name(name)?;
        remove_any(&self.account_dir(i, name))?;
        if self.active(i).as_deref() == Some(name) { self.set_active(i, None)?; }
        Ok(())
    }

    pub fn rename_account(&self, i: Instance, old: &str, new: &str) -> Result<()> {
        validate_name(old)?; validate_name(new)?;
        if self.account_dir(i, new).exists() { bail!("'{new}' already exists"); }
        fs::rename(self.account_dir(i, old), self.account_dir(i, new))?;
        let mp = self.account_dir(i, new).join("meta.json");
        if let Some(mut m) = read_json_obj(&mp) { m.insert("name".into(), new.into()); fs::write(mp, serde_json::to_string_pretty(&m)?)?; }
        if self.active(i).as_deref() == Some(old) { self.set_active(i, Some(new))?; }
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

/// What the process check needs to know about one process.
#[derive(Clone, Debug)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent: Option<u32>,
    pub exe: Option<PathBuf>,
    pub cmd: Vec<String>,
}

fn norm_path(p: &Path) -> String { p.to_string_lossy().replace('\\', "/").to_lowercase() }

/// Pids of Claude Code **CLI** processes among `procs`: a `claude` executable (native install) or a
/// `node`/`bun` running `@anthropic-ai/claude-code` (npm install; IDE extensions use one of these too).
/// `claude.exe` is also what Claude Desktop runs for its own Code tab, so these are NOT counted:
///  * anything that lives in Desktop's data folder (`desktop_dir`, where Desktop downloads its `claude-code`),
///    in the MSIX package folders or under `WindowsApps`,
///  * anything that has a Claude Desktop process among its ancestors.
pub fn cli_pids(procs: &[ProcInfo], desktop_dir: &Path) -> Vec<u32> {
    let by_pid: HashMap<u32, &ProcInfo> = procs.iter().map(|p| (p.pid, p)).collect();
    let is_desktop = |p: &ProcInfo| p.exe.as_deref().map_or(false, is_desktop_exe);
    let dd = norm_path(desktop_dir);
    let dd = if dd.is_empty() { dd } else { format!("{}/", dd.trim_end_matches('/')) };
    let desktop_owned = |exe: &Path| {
        let s = norm_path(exe);
        (!dd.is_empty() && s.starts_with(&dd))
            || s.contains("/windowsapps/")
            || s.contains("anthropicclaude")
            || s.contains("/packages/claude_")
            || s.contains("/packages/anthropicpbc.claude_")
    };
    let has_desktop_ancestor = |p: &ProcInfo| {
        let mut cur = p.parent;
        for _ in 0..64 {
            let Some(pid) = cur else { return false };
            let Some(a) = by_pid.get(&pid) else { return false };
            if is_desktop(a) { return true; }
            cur = a.parent;
        }
        false
    };
    procs
        .iter()
        .filter(|p| {
            let Some(exe) = p.exe.as_deref() else { return false };
            let stem = exe.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
            let looks_like_cli = stem == "claude"
                || (matches!(stem.as_str(), "node" | "bun")
                    && p.cmd.iter().any(|a| a.replace('\\', "/").to_lowercase().contains("@anthropic-ai/claude-code")));
            looks_like_cli && !is_desktop(p) && !desktop_owned(exe) && !has_desktop_ancestor(p)
        })
        .map(|p| p.pid)
        .collect()
}

fn snapshot_procs() -> Vec<ProcInfo> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet).with_cmd(UpdateKind::OnlyIfNotSet),
    );
    sys.processes()
        .iter()
        .map(|(pid, p)| ProcInfo {
            pid: pid.as_u32(),
            parent: p.parent().map(|x| x.as_u32()),
            exe: p.exe().map(Path::to_path_buf),
            cmd: p.cmd().iter().map(|a| a.to_string_lossy().to_string()).collect(),
        })
        .collect()
}

/// Is a Claude Code CLI session running? (Desktop's own Code-tab processes do not count, see `cli_pids`.)
pub fn code_running(desktop_dir: &Path) -> bool { !cli_pids(&snapshot_procs(), desktop_dir).is_empty() }

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
    /// Build a Swapper for this machine (auto-detected paths, real process checks for both instances).
    pub fn detect() -> Result<Swapper> {
        let paths = Paths::detect()?;
        let desktop_dir = paths.desktop.clone();
        Ok(Swapper::new(paths, Box::new(move |i| match i {
            Instance::Desktop => desktop_running(),
            Instance::Code => code_running(&desktop_dir),
        })))
    }

    /// Same as `detect`, with an optional backup limit per account (`None` = default / settings.json).
    pub fn detect_with(max_backups: Option<usize>) -> Result<Swapper> {
        Ok(Self::detect()?.with_max_backups(max_backups))
    }

    /// Only Claude Desktop can be closed for the user; CLI sessions are theirs to close.
    fn close_if_needed(&self, i: Instance, close_first: bool) -> Result<()> {
        if i == Instance::Desktop && close_first && (self.is_running)(i) {
            kill_desktop()?;
        }
        Ok(())
    }

    /// Switch account; Claude Desktop is reopened afterwards (the CLI has nothing to reopen).
    pub fn switch_and_launch(&self, i: Instance, name: &str, close_first: bool) -> Result<()> {
        self.close_if_needed(i, close_first)?;
        self.switch_to(i, name)?;
        if i == Instance::Desktop { launch_desktop(&self.paths.desktop)?; }
        Ok(())
    }

    /// Store the current account and sign out so a new account can log in
    /// (Desktop is opened for that; for the CLI run `claude` and `/login`).
    pub fn start_new_account(&self, i: Instance, close_first: bool) -> Result<()> {
        self.close_if_needed(i, close_first)?;
        self.begin_new_account(i)?;
        if i == Instance::Desktop { launch_desktop(&self.paths.desktop)?; }
        Ok(())
    }

    /// Save the (new) login under `name` and clear the recovery marker.
    pub fn save_login(&self, i: Instance, name: &str, overwrite: bool) -> Result<()> {
        self.save_current(i, name, overwrite)?;
        self.finish_new_account(i);
        Ok(())
    }

    /// Close Claude Desktop (if running and asked to) and undo an interrupted switch / add-account.
    pub fn recover_closing(&self, i: Instance, close_first: bool) -> Result<String> {
        self.close_if_needed(i, close_first)?;
        self.recover(i)
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use Instance::{Code, Desktop};
    use std::sync::{atomic::{AtomicBool, Ordering}, Arc};

    struct Env {
        _t: tempfile::TempDir,
        sw: Swapper,
        /// Claude Desktop "running"
        run: Arc<AtomicBool>,
        /// Claude Code CLI "running"
        crun: Arc<AtomicBool>,
    }

    fn w(p: &Path, s: &str) { fs::create_dir_all(p.parent().unwrap()).unwrap(); fs::write(p, s).unwrap(); }
    fn r(p: &Path) -> String { fs::read_to_string(p).unwrap_or_else(|_| "<missing>".into()) }

    fn env() -> Env {
        let t = tempfile::tempdir().unwrap();
        let b = t.path();
        let run = Arc::new(AtomicBool::new(false));
        let crun = Arc::new(AtomicBool::new(false));
        let (r2, c2) = (run.clone(), crun.clone());
        let paths = Paths { desktop: b.join("desk"), code_dir: b.join("home/.claude"), code_json: b.join("home/.claude.json"), store: b.join("store") };
        let sw = Swapper::new(paths, Box::new(move |i| match i {
            Desktop => r2.load(Ordering::SeqCst),
            Code => c2.load(Ordering::SeqCst),
        }));
        Env { _t: t, sw, run, crun }
    }

    fn login_desktop(e: &Env, who: &str) {
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
    }

    fn login_code(e: &Env, who: &str) {
        let p = &e.sw.paths;
        w(&p.code_dir.join(".credentials.json"), &format!("cred-{who}"));
        w(&p.code_dir.join("settings.json"), &format!("settings-{who}"));
        w(&p.code_dir.join("skills/s1/SKILL.md"), &format!("skill-{who}"));
        w(&p.code_dir.join("projects/p/t.jsonl"), "transcript"); // excluded from switches: shared
        w(&p.code_json, &format!(r#"{{"numStartups":5,"projects":{{"a":1}},"mcpServers":{{"srv":"{who}"}},"oauthAccount":{{"emailAddress":"{who}@x.com"}}}}"#));
    }

    fn login(e: &Env, who: &str) { login_desktop(e, who); login_code(e, who); }

    fn unpack(arch: &str) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(arch).unwrap())).unpack(t.path()).unwrap();
        t
    }

    #[test]
    fn roundtrip_switch_desktop() {
        let e = env();
        login_desktop(&e, "alice");
        e.sw.save_current(Desktop, "Alice", false).unwrap();
        let a = e.sw.account_dir(Desktop, "Alice");
        assert!(!a.join("desktop/Network/lockfile").exists());
        assert!(!a.join("desktop/Cache").exists());
        e.sw.begin_new_account(Desktop).unwrap();
        assert!(!e.sw.signed_in(Desktop));
        login_desktop(&e, "bob");
        e.sw.save_current(Desktop, "Bob", false).unwrap();
        e.sw.finish_new_account(Desktop);
        assert_eq!(e.sw.active(Desktop).as_deref(), Some("Bob"));

        // token refreshed in live bob before switching -> must be kept in Bob's slot
        w(&e.sw.paths.desktop.join("config.json"), "cfg-bob-refreshed");
        e.sw.switch_to(Desktop, "Alice").unwrap();
        assert_eq!(r(&e.sw.paths.desktop.join("config.json")), "cfg-alice");
        assert_eq!(e.sw.active(Desktop).as_deref(), Some("Alice"));
        // MCP + extensions switched too
        assert_eq!(r(&e.sw.paths.desktop.join("claude_desktop_config.json")), "mcp-desktop-alice");
        assert_eq!(r(&e.sw.paths.desktop.join("Claude Extensions/ext/manifest.json")), "ext-alice");
        // excluded items untouched
        assert_eq!(r(&e.sw.paths.desktop.join("vm_bundles/rootfs.vhdx")), "HUGE-VM");
        e.sw.switch_to(Desktop, "Bob").unwrap();
        assert_eq!(r(&e.sw.paths.desktop.join("config.json")), "cfg-bob-refreshed");
        assert!(!e.sw.status().desktop.pending_recovery);
    }

    #[test]
    fn manual_login_does_not_corrupt_the_previous_slot() {
        let e = env();
        login_code(&e, "alice");
        e.sw.save_current(Code, "Alice", false).unwrap();
        // user logs in to bob by hand (`/login`): active is still "Alice"
        login_code(&e, "bob");
        assert_eq!(e.sw.active(Code).as_deref(), Some("Alice"));
        assert_eq!(e.sw.current(Code), None); // bob is not saved
        // back to Alice: must not save bob's login into Alice's slot
        e.sw.switch_to(Code, "Alice").unwrap();
        assert_eq!(r(&e.sw.paths.code_dir.join(".credentials.json")), "cred-alice");
        // save bob, then switch around: each slot keeps its own login
        login_code(&e, "bob");
        e.sw.save_current(Code, "Bob", false).unwrap();
        login_code(&e, "alice");
        e.sw.set_active(Code, Some("Bob")).unwrap(); // stale: alice is live
        e.sw.switch_to(Code, "Bob").unwrap();
        assert_eq!(r(&e.sw.account_dir(Code, "Alice").join("code/claude/.credentials.json")), "cred-alice");
        assert_eq!(r(&e.sw.paths.code_dir.join(".credentials.json")), "cred-bob");
        e.sw.switch_to(Code, "Alice").unwrap();
        assert_eq!(r(&e.sw.paths.code_dir.join(".credentials.json")), "cred-alice");
    }

    #[test]
    fn roundtrip_switch_code() {
        let e = env();
        login_code(&e, "alice");
        e.sw.save_current(Code, "Alice", false).unwrap();
        let a = e.sw.account_dir(Code, "Alice");
        assert_eq!(r(&a.join("code/claude/.credentials.json")), "cred-alice");
        assert!(!a.join("code/claude/projects").exists());
        assert_eq!(e.sw.list(Code)[0].instance, Code);
        e.sw.begin_new_account(Code).unwrap();
        assert!(!e.sw.signed_in(Code));
        // sign-out keeps MCP + skills but drops the login
        assert!(r(&e.sw.paths.code_json).contains("mcpServers") && !r(&e.sw.paths.code_json).contains("oauthAccount"));
        assert_eq!(r(&e.sw.paths.code_dir.join("skills/s1/SKILL.md")), "skill-alice");
        login_code(&e, "bob");
        w(&e.sw.paths.code_json, r#"{"numStartups":9,"mcpServers":{"srv":"bob"},"oauthAccount":{"emailAddress":"bob@x.com"}}"#);
        e.sw.save_current(Code, "Bob", false).unwrap();
        e.sw.finish_new_account(Code);
        assert_eq!(e.sw.active(Code).as_deref(), Some("Bob"));

        e.sw.switch_to(Code, "Alice").unwrap();
        assert_eq!(r(&e.sw.paths.code_dir.join(".credentials.json")), "cred-alice");
        let cj = r(&e.sw.paths.code_json);
        assert!(cj.contains("alice@x.com") && cj.contains("\"numStartups\": 9") && cj.contains("\"srv\": \"alice\""), "{cj}");
        assert_eq!(r(&e.sw.paths.code_dir.join("skills/s1/SKILL.md")), "skill-alice");
        assert_eq!(r(&e.sw.paths.code_dir.join("settings.json")), "settings-alice");
        assert_eq!(r(&e.sw.paths.code_dir.join("projects/p/t.jsonl")), "transcript"); // untouched
        assert_eq!(e.sw.live_email(Code).as_deref(), Some("alice@x.com"));
        e.sw.switch_to(Code, "Bob").unwrap();
        assert!(r(&e.sw.paths.code_json).contains("bob@x.com"));
    }

    #[test]
    fn instances_are_independent() {
        let e = env();
        login(&e, "alice");
        // the same name can exist once per instance, with different content
        e.sw.save_current(Desktop, "Work", false).unwrap();
        e.sw.save_current(Code, "Work", false).unwrap();
        assert!(e.sw.account_dir(Desktop, "Work").join("desktop/config.json").exists());
        assert!(e.sw.account_dir(Code, "Work").join("code/claude/.credentials.json").exists());
        assert_eq!(e.sw.list(Desktop).len(), 1);
        assert_eq!(e.sw.list(Code).len(), 1);

        // Desktop signed in to bob, CLI stays on alice
        login_desktop(&e, "bob");
        e.sw.save_current(Desktop, "Personal", false).unwrap();
        assert_eq!(e.sw.active(Desktop).as_deref(), Some("Personal"));
        assert_eq!(e.sw.active(Code).as_deref(), Some("Work"));

        // CLI running does not block Desktop and the other way round
        e.crun.store(true, Ordering::SeqCst);
        e.sw.switch_to(Desktop, "Work").unwrap();
        assert!(e.sw.save_current(Code, "X", false).is_err());
        assert!(e.sw.switch_to(Code, "Work").is_err());
        e.crun.store(false, Ordering::SeqCst);
        e.run.store(true, Ordering::SeqCst);
        e.sw.switch_to(Code, "Work").unwrap();
        e.sw.begin_new_account(Code).unwrap();
        assert!(e.sw.switch_to(Desktop, "Personal").is_err());
        e.run.store(false, Ordering::SeqCst);

        // operations on one instance never touched the other's files
        assert_eq!(r(&e.sw.paths.desktop.join("config.json")), "cfg-alice");
        assert!(!e.sw.signed_in(Code));
        // each instance keeps its own recovery marker
        let st = e.sw.status();
        assert!(st.code.pending_recovery && !st.desktop.pending_recovery);
        e.sw.recover(Code).unwrap();
        assert!(e.sw.signed_in(Code));
        assert_eq!(e.sw.active(Desktop).as_deref(), Some("Work"));

        // deleting / renaming only affects the chosen instance
        e.sw.rename_account(Code, "Work", "Job").unwrap();
        assert_eq!(e.sw.active(Code).as_deref(), Some("Job"));
        assert_eq!(e.sw.active(Desktop).as_deref(), Some("Work"));
        assert_eq!(e.sw.list(Desktop)[0].name, "Personal");
        e.sw.delete_account(Code, "Job").unwrap();
        assert_eq!(e.sw.list(Code).len(), 0);
        assert_eq!(e.sw.list(Desktop).len(), 2);
        assert_eq!(e.sw.active(Code), None);
    }

    #[test]
    fn backups_are_tagged_targz_per_account_and_restorable() {
        let e = env();
        login(&e, "alice");
        e.sw.save_current(Desktop, "Alice", false).unwrap();
        e.sw.save_current(Code, "Alice", false).unwrap();
        e.sw.begin_new_account(Desktop).unwrap(); // backup belongs to active = Alice
        let b = e.sw.list_backups();
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].owner.as_str(), b[0].instance), ("Alice", Desktop));
        assert!(b[0].path.to_string_lossy().ends_with(".tar.gz") && b[0].path.is_file());
        assert!(b[0].path.file_name().unwrap().to_string_lossy().contains("-desktop-"));
        // no leftover temp dirs
        assert!(fs::read_dir(&e.sw.paths.store).unwrap().flatten().all(|d| !d.file_name().to_string_lossy().starts_with(".tmp-")));
        // the archive is tagged inside as well, and holds only that instance
        let t = unpack(&b[0].path.to_string_lossy());
        assert_eq!(read_meta(t.path()).instance, Desktop);
        assert!(t.path().join("desktop").is_dir() && !t.path().join("code").exists());

        std::thread::sleep(std::time::Duration::from_millis(2));
        let rep = e.sw.backup_now(Code).unwrap();
        assert_eq!(rep.instance, Code);
        assert!(rep.archive.contains("-code-"));
        let t = unpack(&rep.archive);
        assert_eq!(read_meta(t.path()).instance, Code);
        assert!(t.path().join("code/claude").is_dir() && !t.path().join("desktop").exists());
        let all = e.sw.list_backups();
        assert_eq!((all[0].instance, all[1].instance), (Code, Desktop)); // newest first

        // restore: Desktop archive only changes Desktop
        login(&e, "zed");
        e.sw.restore_backup(&b[0].path).unwrap();
        assert_eq!(r(&e.sw.paths.desktop.join("config.json")), "cfg-alice");
        assert_eq!(r(&e.sw.paths.code_dir.join(".credentials.json")), "cred-zed");
        e.sw.restore_backup(Path::new(&rep.archive)).unwrap();
        assert_eq!(r(&e.sw.paths.code_dir.join(".credentials.json")), "cred-alice");
        // restoring refuses only while THE archive's instance is running
        e.run.store(true, Ordering::SeqCst);
        e.sw.restore_backup(Path::new(&rep.archive)).unwrap();
        assert!(e.sw.restore_backup(&b[0].path).is_err());
    }

    #[test]
    fn old_backup_names_are_desktop() {
        assert_eq!(backup_instance(Path::new("1700000000000-manual.tar.gz")), Desktop);
        assert_eq!(backup_instance(Path::new("1700000000000-switch-to-code.tar.gz")), Desktop);
        assert_eq!(backup_instance(Path::new("1700000000000-code-manual.tar.gz")), Code);
        assert_eq!(backup_instance(Path::new("1700000000000-desktop-manual-live.tar.gz")), Desktop);
    }

    #[test]
    fn prunes_old_backups_to_the_default() {
        let e = env();
        login(&e, "alice");
        assert_eq!(e.sw.max_backups(), DEFAULT_MAX_BACKUPS_PER_ACCOUNT);
        for k in 0..DEFAULT_MAX_BACKUPS_PER_ACCOUNT + 4 {
            e.sw.backup_live(Desktop, &format!("t{k}")).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(e.sw.list_backups().len(), DEFAULT_MAX_BACKUPS_PER_ACCOUNT);
    }

    #[test]
    fn overwrite_refuses_a_different_account() {
        let e = env();
        login_code(&e, "alice");
        e.sw.save_current(Code, "work", false).unwrap();
        // a manual /login as someone else must not replace the saved slot
        login_code(&e, "bob");
        assert!(e.sw.save_current(Code, "work", true).is_err());
        let slot = e.sw.list(Code).into_iter().find(|m| m.name == "work").unwrap();
        assert_eq!(slot.email.as_deref(), Some("alice@x.com"));
        // the same account signing in again may refresh its slot
        login_code(&e, "alice");
        e.sw.save_current(Code, "work", true).unwrap();
    }

    #[test]
    fn prune_counts_each_instance_separately() {
        let e = env();
        login_code(&e, "alice");
        w(&e.sw.paths.desktop.join("config.json"), "c");
        e.sw.save_settings(&Settings { max_backups: Some(3), ..Default::default() }).unwrap();
        // same account name for both instances -> same backup folder
        e.sw.set_active(Desktop, Some("Work")).unwrap();
        e.sw.set_active(Code, Some("Work")).unwrap();
        for k in 0..6 {
            e.sw.backup_live(Desktop, &format!("d{k}")).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
            e.sw.backup_live(Code, &format!("c{k}")).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let l = e.sw.list_backups();
        assert_eq!(l.iter().filter(|b| b.instance == Desktop).count(), 3);
        assert_eq!(l.iter().filter(|b| b.instance == Code).count(), 3);
        assert!(l.iter().all(|b| b.owner == "Work"));
        // explicit parameter wins over settings; 0 is clamped to 1
        let sw = Swapper::new(e.sw.paths.clone(), Box::new(|_| false)).with_max_backups(Some(2));
        sw.backup_live(Code, "p").unwrap();
        assert_eq!(sw.list_backups().iter().filter(|b| b.instance == Code).count(), 2);
        assert_eq!(sw.list_backups().iter().filter(|b| b.instance == Desktop).count(), 3);
        assert_eq!(Swapper::new(e.sw.paths.clone(), Box::new(|_| false)).with_max_backups(Some(0)).max_backups(), 1);
    }

    #[test]
    fn unassigned_backup_folders_use_the_instance_email() {
        let e = env();
        login_code(&e, "alice");
        e.sw.backup_live(Code, "t").unwrap();
        assert_eq!(e.sw.list_backups()[0].owner, "alice_at_x.com_unassigned");
        // nobody signed in at all (Desktop e-mail unknown) -> plain label
        w(&e.sw.paths.desktop.join("config.json"), "c");
        e.sw.backup_live(Desktop, "t").unwrap();
        assert!(e.sw.list_backups().iter().any(|b| b.instance == Desktop && b.owner == "_unassigned"));
    }

    #[test]
    fn snapshot_excludes_junk_and_old_format_is_rejected() {
        let e = env();
        login(&e, "alice");
        e.sw.save_current(Desktop, "Alice", false).unwrap();
        e.sw.save_current(Code, "Alice", false).unwrap();
        let a = e.sw.account_dir(Desktop, "Alice");
        let c = e.sw.account_dir(Code, "Alice");
        assert!(a.join("desktop/claude_desktop_config.json").exists());
        assert!(!a.join("code").exists() && !c.join("desktop").exists()); // one instance per slot
        assert!(c.join("code/claude/skills/s1/SKILL.md").exists());
        assert!(!a.join("desktop/vm_bundles").exists());
        assert!(!a.join("desktop/Cache").exists());
        assert!(!c.join("code/claude/projects").exists());
        // pretend it was saved by v1
        fs::write(a.join("meta.json"), r#"{"name":"Alice"}"#).unwrap();
        assert!(e.sw.switch_to(Desktop, "Alice").is_err());
        // ... and live files were not touched by the refused switch? (backup taken, then restore fails -> rolled back)
        assert_eq!(r(&e.sw.paths.desktop.join("config.json")), "cfg-alice");
        // a v2 slot (no instance, format 2) is a Desktop slot and still restorable
        fs::write(a.join("meta.json"), r#"{"name":"Alice","email":null,"saved_at_ms":1,"format":2,"has_desktop":true,"has_code":true}"#).unwrap();
        assert_eq!(e.sw.list(Desktop)[0].instance, Desktop);
        login_desktop(&e, "bob");
        e.sw.switch_to(Desktop, "Alice").unwrap();
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
        e.sw.set_threshold_mb(0).unwrap();
        let l = e.sw.scan_large();
        assert!(l.iter().any(|i| i.root == "desktop" && i.name == "NewBigThing" && i.bytes == 3000));
        assert!(l.iter().any(|i| i.root == "code" && i.name == "plugins"));
        assert!(!l.iter().any(|i| i.name == "vm_bundles" || i.name == "Cache")); // excluded never reported
        assert!(l.iter().any(|i| i.name == "projects")); // session data is part of backups -> reported when large
        // keep -> no more warning, still included in snapshots
        e.sw.acknowledge_item("desktop", "NewBigThing").unwrap();
        assert!(!e.sw.scan_large().iter().any(|i| i.name == "NewBigThing"));
        e.sw.save_current(Desktop, "A", false).unwrap();
        assert!(e.sw.account_dir(Desktop, "A").join("desktop/NewBigThing/data.bin").exists());
        // exclude -> not warned, not snapshotted
        e.sw.exclude_item("code", "plugins").unwrap();
        assert!(!e.sw.scan_large().iter().any(|i| i.name == "plugins"));
        e.sw.save_current(Code, "A", false).unwrap();
        assert!(!e.sw.account_dir(Code, "A").join("code/claude/plugins").exists());
    }

    #[test]
    fn manual_backup_while_running_is_lenient() {
        let e = env();
        login_desktop(&e, "alice");
        e.sw.save_current(Desktop, "Alice", false).unwrap();
        // strict when closed
        let r1 = e.sw.backup_now(Desktop).unwrap();
        assert!(!r1.live && r1.skipped.is_empty() && r1.archive.contains("Alice") && r1.archive.ends_with(".tar.gz"));
        // simulate a file we cannot read while Claude runs: a dangling symlink
        #[cfg(unix)]
        std::os::unix::fs::symlink("/nonexistent/locked", e.sw.paths.desktop.join("Local Storage/locked.ldb")).unwrap_or_else(|_| {
            fs::create_dir_all(e.sw.paths.desktop.join("Local Storage")).unwrap();
            std::os::unix::fs::symlink("/nonexistent/locked", e.sw.paths.desktop.join("Local Storage/locked.ldb")).unwrap();
        });
        e.run.store(true, Ordering::SeqCst); // Claude running
        let r2 = e.sw.backup_now(Desktop).unwrap();
        assert!(r2.live);
        #[cfg(unix)]
        assert!(r2.skipped.iter().any(|x| x.contains("locked.ldb")), "{:?}", r2.skipped);
        // only Desktop is "running": a CLI backup is a strict one
        login_code(&e, "alice");
        assert!(!e.sw.backup_now(Code).unwrap().live);
        // the backup is still restorable (live files untouched, switching still refused while running)
        assert!(e.sw.switch_to(Desktop, "Alice").is_err());
        assert_eq!(e.sw.list_backups().len(), 3);
        // while closed, a bad file makes a strict backup fail instead of silently skipping
        #[cfg(unix)]
        { e.run.store(false, Ordering::SeqCst); assert!(e.sw.backup_now(Desktop).is_err()); }
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
        assert_eq!(e.sw.live_email(Desktop).as_deref(), Some("fsadannn@gmail.com"));
        assert_eq!(email_in_blob(b"email_address\"\x03abc"), None);
        // the CLI e-mail comes from ~/.claude.json, never from Desktop's files
        assert_eq!(e.sw.live_email(Code), None);
        login_code(&e, "carol");
        assert_eq!(e.sw.live_email(Code).as_deref(), Some("carol@x.com"));
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
        // old settings.json files (with the removed manage_* keys) still load, new switch defaults to on
        w(&e.sw.paths.store.join("settings.json"), r#"{"threshold_mb":9,"manage_desktop":true,"manage_code":false}"#);
        let st = e.sw.settings();
        assert_eq!(st.threshold_mb, 9);
        assert!(st.backup_code_sessions);
    }

    #[test]
    fn backups_include_sessions_but_switching_leaves_them_alone() {
        let e = env();
        login(&e, "alice");
        e.sw.save_current(Desktop, "Alice", false).unwrap();
        e.sw.save_current(Code, "Alice", false).unwrap();
        // not in the account slot
        assert!(!e.sw.account_dir(Desktop, "Alice").join("desktop/local-agent-mode-sessions").exists());
        assert!(!e.sw.account_dir(Desktop, "Alice").join("desktop/claude-code-sessions").exists());
        // but in the backup archive (incl. Claude Code projects/history)
        w(&e.sw.paths.code_dir.join("history.jsonl"), "hist");
        let rep = e.sw.backup_now(Desktop).unwrap();
        let t = unpack(&rep.archive);
        assert_eq!(r(&t.path().join("desktop/local-agent-mode-sessions/s1/x.json")), "cowork-alice");
        assert_eq!(r(&t.path().join("desktop/claude-code-sessions/s1/y.json")), "ccsess-alice");
        assert!(!t.path().join("desktop/vm_bundles").exists() && !t.path().join("desktop/Cache").exists());
        let rep_c = e.sw.backup_now(Code).unwrap();
        let tc = unpack(&rep_c.archive);
        assert_eq!(r(&tc.path().join("code/claude/projects/p/t.jsonl")), "transcript");
        assert_eq!(r(&tc.path().join("code/claude/history.jsonl")), "hist");
        // switching never touches live sessions
        login_desktop(&e, "bob");
        e.sw.save_current(Desktop, "Bob", false).unwrap();
        w(&e.sw.paths.desktop.join("local-agent-mode-sessions/s1/x.json"), "NEWER");
        e.sw.switch_to(Desktop, "Alice").unwrap();
        assert_eq!(r(&e.sw.paths.desktop.join("local-agent-mode-sessions/s1/x.json")), "NEWER");
        // explicit backup restore brings sessions back
        e.sw.restore_backup(Path::new(&rep.archive)).unwrap();
        assert_eq!(r(&e.sw.paths.desktop.join("local-agent-mode-sessions/s1/x.json")), "cowork-alice");
        w(&e.sw.paths.code_dir.join("history.jsonl"), "changed");
        e.sw.restore_backup(Path::new(&rep_c.archive)).unwrap();
        assert_eq!(r(&e.sw.paths.code_dir.join("history.jsonl")), "hist");
    }

    #[test]
    fn cli_session_data_switch_for_backups() {
        let e = env();
        login_code(&e, "alice");
        w(&e.sw.paths.code_dir.join("history.jsonl"), "hist");
        w(&e.sw.paths.code_dir.join("todos/t.json"), "todo");
        w(&e.sw.paths.code_dir.join("plans/p.md"), "plan");
        assert!(e.sw.settings().backup_code_sessions); // on by default
        let has = |arch: &str| {
            let t = unpack(arch);
            CODE_SESSION_ITEMS.iter().map(|n| t.path().join("code/claude").join(n).exists()).collect::<Vec<_>>()
        };
        assert_eq!(has(&e.sw.backup_now(Code).unwrap().archive), [true; 4]);
        e.sw.set_backup_code_sessions(false).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let off = e.sw.backup_now(Code).unwrap();
        assert_eq!(has(&off.archive), [false; 4]);
        // everything else is still in there, and the large-item scan no longer reports session data
        assert!(unpack(&off.archive).path().join("code/claude/skills/s1/SKILL.md").exists());
        e.sw.set_threshold_mb(0).unwrap();
        assert!(!e.sw.scan_large().iter().any(|i| i.root == "code" && CODE_SESSION_ITEMS.contains(&i.name.as_str())));
        // the switch has no effect on Desktop backups
        login_desktop(&e, "alice");
        assert!(unpack(&e.sw.backup_now(Desktop).unwrap().archive).path().join("desktop/local-agent-mode-sessions").exists());
        // restoring a backup that has sessions still works with the switch off
        e.sw.set_backup_code_sessions(true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let on = e.sw.backup_now(Code).unwrap();
        e.sw.set_backup_code_sessions(false).unwrap();
        w(&e.sw.paths.code_dir.join("history.jsonl"), "changed");
        e.sw.restore_backup(Path::new(&on.archive)).unwrap();
        assert_eq!(r(&e.sw.paths.code_dir.join("history.jsonl")), "hist");
    }

    #[test]
    fn git_toggle_and_full_restore() {
        let e = env();
        login_desktop(&e, "alice");
        let p = e.sw.paths.clone();
        w(&p.desktop.join("git-shadow/repo/obj"), "G1");
        w(&p.desktop.join("git-worktrees.json"), "W1");
        w(&p.desktop.join("scratch-workspaces/a/f"), "S1");
        e.sw.save_current(Desktop, "Alice", false).unwrap();
        // toggle off (default): git data not in backups, not in slots
        assert!(!e.sw.settings().backup_git);
        let r1 = e.sw.backup_now(Desktop).unwrap();
        let t1 = unpack(&r1.archive);
        assert!(!t1.path().join("desktop/git-shadow").exists() && !t1.path().join("desktop/scratch-workspaces").exists());
        assert!(!t1.path().join("desktop/git-worktrees.json").exists());
        assert!(!e.sw.scan_large().iter().any(|i| i.name == "git-shadow"));
        // toggle on: included
        e.sw.set_backup_git(true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        let r2 = e.sw.backup_now(Desktop).unwrap();
        let t2 = unpack(&r2.archive);
        assert_eq!(r(&t2.path().join("desktop/git-shadow/repo/obj")), "G1");
        assert_eq!(r(&t2.path().join("desktop/git-worktrees.json")), "W1");
        assert_eq!(r(&t2.path().join("desktop/scratch-workspaces/a/f")), "S1");
        // never part of a switch slot, even when on
        assert!(!e.sw.account_dir(Desktop, "Alice").join("desktop/git-shadow").exists());

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
    fn one_instance_never_reads_or_touches_the_other() {
        let e = env();
        login(&e, "alice");
        let p = e.sw.paths.clone();
        let code_before = (r(&p.code_json), r(&p.code_dir.join(".credentials.json")), r(&p.code_dir.join("skills/s1/SKILL.md")));
        e.sw.save_current(Desktop, "Alice", false).unwrap();
        login_desktop(&e, "bob");
        e.sw.save_current(Desktop, "Bob", false).unwrap();
        e.sw.switch_to(Desktop, "Alice").unwrap();
        e.sw.begin_new_account(Desktop).unwrap();
        let rep = e.sw.backup_now(Desktop).unwrap();
        e.sw.recover(Desktop).unwrap();
        assert_eq!(r(&p.desktop.join("config.json")), "cfg-alice");
        assert_eq!((r(&p.code_json), r(&p.code_dir.join(".credentials.json")), r(&p.code_dir.join("skills/s1/SKILL.md"))), code_before);
        let t = unpack(&rep.archive);
        assert!(!t.path().join("code").exists());
        assert_eq!(e.sw.active(Code), None);
        // and the other way round
        e.sw.save_current(Code, "Alice", false).unwrap();
        login_code(&e, "bob");
        e.sw.save_current(Code, "Bob", false).unwrap();
        let desk_before = r(&p.desktop.join("config.json"));
        e.sw.switch_to(Code, "Alice").unwrap();
        e.sw.begin_new_account(Code).unwrap();
        e.sw.recover(Code).unwrap();
        assert_eq!(r(&p.desktop.join("config.json")), desk_before);
        assert!(p.desktop.join("Network/Cookies").exists());
    }

    #[test]
    fn refuses_when_running() {
        let e = env();
        login(&e, "alice");
        e.run.store(true, Ordering::SeqCst);
        assert!(e.sw.save_current(Desktop, "A", false).is_err());
        assert!(e.sw.save_current(Code, "A", false).is_ok());
        e.run.store(false, Ordering::SeqCst);
        e.crun.store(true, Ordering::SeqCst);
        assert!(e.sw.save_current(Desktop, "A", false).is_ok());
        let msg = e.sw.begin_new_account(Code).unwrap_err().to_string();
        assert!(msg.contains("CLI"), "{msg}");
    }

    #[test]
    fn recover_after_cancel() {
        let e = env();
        login(&e, "alice");
        // recover finds the active account again by the signed-in e-mail
        let d = e.sw.paths.desktop.join("IndexedDB/https_claude.ai_0.indexeddb.blob/5/01");
        let mut blob = b"email_address\"".to_vec();
        blob.push(11);
        blob.extend_from_slice(b"alice@x.com");
        w(&d.join("1"), "");
        fs::write(d.join("1"), blob).unwrap();
        for i in Instance::ALL {
            e.sw.save_current(i, "Alice", false).unwrap();
            e.sw.begin_new_account(i).unwrap();
            assert!(e.sw.status().get(i).pending_recovery);
            e.sw.recover(i).unwrap();
            assert_eq!(e.sw.active(i).as_deref(), Some("Alice"));
            assert!(!e.sw.status().get(i).pending_recovery);
        }
        assert_eq!(r(&e.sw.paths.desktop.join("config.json")), "cfg-alice");
        assert_eq!(r(&e.sw.paths.code_dir.join(".credentials.json")), "cred-alice");
    }

    #[test]
    fn old_store_layout_and_state_are_migrated() {
        let t = tempfile::tempdir().unwrap();
        let b = t.path();
        let paths = Paths { desktop: b.join("desk"), code_dir: b.join("home/.claude"), code_json: b.join("home/.claude.json"), store: b.join("store") };
        let meta = |n: &str| format!(r#"{{"name":"{n}","email":null,"saved_at_ms":1,"format":2,"has_desktop":true,"has_code":false}}"#);
        // v2 store: slots directly in accounts/, one of them is called "desktop", one "code"
        for n in ["Work", "desktop", "code"] {
            w(&paths.store.join("accounts").join(n).join("meta.json"), &meta(n));
            w(&paths.store.join("accounts").join(n).join("desktop/config.json"), &format!("cfg-{n}"));
        }
        w(&paths.store.join("state.json"), r#"{"active":"Work"}"#);
        w(&paths.store.join("pending.json"), r#"{"backup":"x"}"#);
        let sw = Swapper::new(paths.clone(), Box::new(|_| false));
        let names: Vec<String> = sw.list(Desktop).into_iter().map(|m| m.name).collect();
        assert_eq!(names, ["code", "desktop", "Work"]);
        assert!(sw.list(Code).is_empty());
        assert_eq!(r(&sw.account_dir(Desktop, "desktop").join("desktop/config.json")), "cfg-desktop");
        assert_eq!(r(&sw.account_dir(Desktop, "Work").join("desktop/config.json")), "cfg-Work");
        assert!(!paths.store.join("accounts/Work").exists());
        // old single-string state = Desktop; setting the CLI account keeps it
        assert_eq!(sw.active(Desktop).as_deref(), Some("Work"));
        assert_eq!(sw.active(Code), None);
        sw.set_active(Code, Some("Job")).unwrap();
        assert_eq!((sw.active(Desktop).as_deref(), sw.active(Code).as_deref()), (Some("Work"), Some("Job")));
        sw.set_active(Desktop, None).unwrap();
        assert_eq!((sw.active(Desktop), sw.active(Code).as_deref()), (None, Some("Job")));
        // old pending marker belongs to Desktop
        assert!(sw.status().desktop.pending_recovery && !sw.status().code.pending_recovery);
        // idempotent
        let sw = Swapper::new(paths, Box::new(|_| false));
        assert_eq!(sw.list(Desktop).len(), 3);
    }

    fn p(pid: u32, parent: Option<u32>, exe: &str, cmd: &[&str]) -> ProcInfo {
        ProcInfo { pid, parent, exe: Some(PathBuf::from(exe)), cmd: cmd.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn cli_detection_ignores_what_belongs_to_desktop() {
        let dd = Path::new(r"C:\Users\f\AppData\Local\Packages\Claude_abc\LocalCache\Roaming\Claude");
        let desktop = r"C:\Program Files\WindowsApps\Claude_1.0_x64__abc\app\Claude.exe";
        let procs = vec![
            p(1, None, r"C:\Windows\explorer.exe", &[]),
            p(10, Some(1), desktop, &[]),
            // Desktop's Code tab: claude.exe from Desktop's data folder, child of Desktop
            p(11, Some(10), r"C:\Users\f\AppData\Local\Packages\Claude_abc\LocalCache\Roaming\Claude\claude-code\2.1.0\claude.exe", &[]),
            // a sub-process of that one, started from some other path, still has Desktop as an ancestor
            p(12, Some(11), r"C:\tools\claude.exe", &[]),
            // Desktop's own helper processes
            p(13, Some(10), desktop, &["--type=renderer"]),
            // node started by Desktop for something claude-code related
            p(14, Some(10), r"C:\node\node.exe", &[r"C:\x\node_modules\@anthropic-ai\claude-code\cli.js"]),
            // unrelated stuff
            p(20, Some(1), r"C:\Windows\System32\cmd.exe", &[]),
            p(21, Some(20), r"C:\Program Files\nodejs\node.exe", &["server.js"]),
            p(22, Some(20), r"C:\Users\f\.vscode\claude-swap.exe", &[]),
        ];
        assert!(cli_pids(&procs, dd).is_empty(), "{:?}", cli_pids(&procs, dd));

        let mut more = procs.clone();
        // native install, started from a terminal
        more.push(p(30, Some(20), r"C:\Users\f\.local\bin\claude.exe", &["claude"]));
        // npm install: node running the claude-code package
        more.push(p(31, Some(20), r"C:\Program Files\nodejs\node.exe", &[r"C:\Users\f\AppData\Roaming\npm\node_modules\@anthropic-ai\claude-code\cli.js"]));
        // orphan (parent gone) with no exe info is ignored, with exe info it counts
        more.push(ProcInfo { pid: 32, parent: Some(999), exe: None, cmd: vec![] });
        more.push(p(33, Some(999), r"D:\bin\claude", &[]));
        // classic %APPDATA%\Claude install of Desktop (no MSIX): its claude-code is still Desktop's
        let classic = Path::new(r"C:\Users\f\AppData\Roaming\Claude");
        more.push(p(40, Some(1), r"C:\Users\f\AppData\Roaming\Claude\claude-code\2.1.0\claude.exe", &[]));
        let mut got = cli_pids(&more, dd);
        got.sort();
        assert_eq!(got, [30, 31, 33, 40]); // 40 is only Desktop's when we know Desktop's folder is `classic`
        let mut got = cli_pids(&more, classic);
        got.sort();
        assert_eq!(got, [30, 31, 33]);
    }

    #[test]
    fn names_and_exe() {
        assert!(validate_name("../x").is_err());
        assert!(validate_name("Work 2").is_ok());
        assert!(is_desktop_exe(Path::new(r"C:\Program Files\WindowsApps\Claude_1.0_x64__abc\app\Claude.exe")));
        assert!(!is_desktop_exe(Path::new(r"C:\Users\f\.local\bin\claude.exe")));
        assert_eq!(Instance::from_key("code"), Some(Code));
        assert_eq!((Desktop.label(), Code.label()), ("Desktop", "CLI"));
    }
}
