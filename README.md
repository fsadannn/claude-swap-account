# Claude Swap

A small personal Rust app to **back up and switch Claude Desktop accounts, one at a time** (no parallel accounts).
Typical use: sign in to your **work** account while working, and when you finish, switch to your **personal** account
(and back the next day) without signing in and out by hand.

> **Scope today: Claude Desktop only.** Claude Code (CLI / `~/.claude`) is *not* handled yet – see [TODO](#todo).
> Nothing under `~/.claude` or `~/.claude.json` is read, copied or changed.

```
core/   swap-core   – library with all the logic (paths, snapshots, backups, recovery, process kill / launch) + tests
app/    claude-swap – iced 0.14 GUI; only calls swap-core
```

## Features
- **Accounts**: save the account Claude Desktop is signed in to under a name, then **Switch** between saved accounts
  (Claude is closed during a switch and reopened afterwards). Before a switch the current account is saved back to its
  slot (so refreshed tokens are kept) and a backup is taken.
- **+ Add new account**: backs up and signs Claude out (MCP config / extensions stay), opens Claude; sign in, close Claude,
  press *Save current login*.
- **Back up now**: backs up the current state without switching, also while Claude is running (locked files are skipped
  and reported, so a live backup is best-effort; close Claude for an exact one).
- **Recover**: undoes an interrupted switch / add-account (crash-recovery marker `pending.json`).
- **Settings** tab: max backups per account (empty = default 15), large-item threshold (MB), include git data in backups
  (off by default), open the app / backups folder, undo excluded / kept items.
- **Large-item alert**: warns when a folder in Claude's data dir that goes into backups exceeds the threshold
  (default 100 MB) – e.g. something big Anthropic adds later. *Exclude* it or *Keep* it.

## What is backed up / switched (Claude Desktop data folder)
`%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude` (MSIX; falls back to `%APPDATA%\Claude`).
Both modes are "whole folder minus excludes", so new files are picked up automatically.

| | Account slots (switch) | Backups |
|---|---|---|
| Sign-in (`config.json`, `Local State`, `Network/`, storage, …) | yes | yes |
| MCP config `claude_desktop_config.json`, `Claude Extensions` | yes | yes |
| Sessions (`local-agent-mode-sessions`, `claude-code-sessions`, uploads) | no (left as they are, see note) | yes |
| Git data (`git-shadow`, `git-worktrees.json`, `scratch-workspaces`) | no | only if *Include git data* is on |
| Caches, logs, `vm_bundles` / Cowork VM, `claude-code` binaries, lock files | no | no |

**Why sessions are not part of an account slot:** Claude already keeps them apart per account. Inside
`local-agent-mode-sessions` and `claude-code-sessions` the folders are `<account id>/<organization id>/…`, and Claude only
shows the sessions of the account that is signed in (verified on a real work + personal pair: each account sees only its
own sessions after a switch). So the switch only has to change the identity files (`config.json`, `Local State`,
`cowork-enabled-cli-ops.json`, `Network/`, storage); both accounts' sessions stay on disk side by side, and backups
include all of them.

Lists: `DESKTOP_EXCLUDE`, `DESKTOP_BACKUP_EXCLUDE`, `DESKTOP_GIT_ITEMS` in `core/src/lib.rs`.
The Cowork VM needs nothing special: only the one default Claude data folder is ever used (no extra profiles).

**Restore**: restoring a backup puts back *everything inside that archive* (whatever the exclude / git settings are now)
and never deletes live data that is not in it. Switch / rollback / recover only touch the switch scope.

## Where things are stored
`%LOCALAPPDATA%\claude-swap\`
```
accounts/<name>/…                       account snapshots (plain folders)
backups/<account>/<ms>-<reason>.tar.gz  backups; <account> = active account, else "<email>_unassigned" / "_unassigned"
settings.json   state.json   pending.json
```
The newest N backups are kept per account folder: `Swapper::with_max_backups(Some(n))` / `detect_with(Some(n))`
> `max_backups` in `settings.json` > default 15.

## Safety
Claude Desktop must be closed for a switch (the app can close it for you). Every overwrite is preceded by a backup;
a failed switch is rolled back from it.

## Build / run (Windows)
```
cargo test -p swap-core                  # logic tests (run on any OS)
cargo run -p claude-swap                 # debug GUI
cargo build -p claude-swap --release     # -> target\release\claude-swap.exe
```
Needs only Rust (rustup) – no Node, no WebView.

## Installer (NSIS)
```
winget install NSIS.NSIS        # once
.\installer\build.ps1           # cargo build --release + makensis -> dist\claude-swap-setup-<version>.exe
```
Per-user install (no admin) to `%LOCALAPPDATA%\Programs\Claude Swap`, Start Menu shortcut, entry in *Apps > Installed apps*.
Asks to close a running copy. Uninstall keeps `%LOCALAPPDATA%\claude-swap` (accounts, backups) unless you answer *Yes*
(silent `/S` uninstall never deletes it). The version comes from `Cargo.toml`. Script: `installer/claude-swap.nsi`.

## First use (work <-> personal)
1. Signed in to your work account → close Claude → name it `work` → *Save current login*.
2. *+ Add new account* → Claude opens signed out → sign in to your personal account → close Claude → name it
   `personal` → *Save current login*.
3. From then on, when you finish work press *Switch* on `personal` (and *Switch* on `work` when you start). Claude is
   closed and reopened for you; the account you leave is saved first, so refreshed tokens are kept, and a backup is taken.

## TODO
- [ ] **Claude Code (CLI / `~/.claude`) support.** The library already has it (`Settings::manage_code`, `Scope`,
      `CODE_EXCLUDE`, `CODE_BACKUP_EXCLUDE`, `CODE_JSON_KEYS`; covered by tests) but it is off by default and hidden in the GUI.
      Before enabling: verify on a real machine which files hold the login (`~/.claude/.credentials.json`,
      `oauthAccount` in `~/.claude.json`), what is safe to swap (skills, commands, agents, plugins, `mcpServers`),
      detect running Claude Code CLI sessions (today only Desktop processes are checked), add the GUI switch
      (Desktop / Code) and show the account e-mail (Desktop's e-mail is read from its IndexedDB blobs).
- [ ] GUI button to list backups and *Restore* one (`Swapper::list_backups` / `restore_backup` exist).
- [ ] Verify on Windows: live backup with locked files, the Open Claude launch (AUMID lookup), whether Cowork sessions are per account.
- [ ] Optional: store account slots as `.tar.gz` too.

## License
Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this
project, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
