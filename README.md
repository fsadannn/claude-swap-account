# Claude Swap

A small personal Rust app to **back up and switch Claude accounts, one at a time per instance** – for **Claude Desktop** and
the **Claude Code CLI**. Typical use: sign in to your **work** account while working, and when you finish, switch to your
**personal** account (and back the next day) without signing in and out by hand.

Desktop and the CLI can run at the same time and are signed in independently, so they are two separate **instances**:
each has its own saved accounts, its own active account and its own backups, and everything is tagged **Desktop** or
**CLI** in the UI and in the stored metadata. Working on one never reads or touches the other.

> **Windows only.** Claude Swap targets Claude Desktop for Windows (the Microsoft Store / MSIX install, with a fallback to
> the classic `%APPDATA%\Claude` folder) and the Claude Code CLI in `~/.claude`. Finding and closing Claude, reopening it
> (AUMID lookup) and the installer are Windows-specific; macOS and Linux are not supported. Only the logic tests in
> `swap-core` run on other systems.

```
core/   swap-core   – library with all the logic (paths, snapshots, backups, recovery, process kill / launch) + tests
app/    claude-swap – iced 0.14 GUI; only calls swap-core
```

## Features
- **Accounts**: save the account an instance is signed in to under a name, then **Switch** between saved accounts of that
  instance. Desktop is closed during a switch and reopened afterwards; for the CLI all sessions must be closed (the app
  does not kill terminals). Before a switch the current account is saved back to its slot (so refreshed tokens are kept)
  and a backup is taken. The same name can exist once per instance (e.g. `work` for Desktop and `work` for CLI).
- **+ Add new account**: pick *Desktop* or *CLI* in the `For:` selector. Backs up and signs that instance out (MCP config /
  extensions / skills stay). Desktop: Claude opens, sign in, close Claude. CLI: run `claude` and log in, close the
  sessions. Then press *Save current login*.
- **Back up now** (one button per instance): backs up the current state without switching, also while that Claude is
  running (locked files are skipped and reported, so a live backup is best-effort; close it for an exact one).
- **Recover**: undoes an interrupted switch / add-account of that instance (crash-recovery markers `pending.json` for
  Desktop, `pending-code.json` for the CLI).
- **Settings** tab: max backups per account (empty = default 15, counted per instance), large-item threshold (MB), include
  Desktop git data in backups (off by default), include CLI sessions in backups (on by default), open the app / backups
  folder, undo excluded / kept items.
- **Large-item alert**: warns when a folder in either data dir that goes into backups exceeds the threshold
  (default 100 MB) – e.g. something big Anthropic adds later. *Exclude* it or *Keep* it.

## Telling Desktop and the CLI apart
`claude.exe` is both Desktop's helper and the CLI. A process counts as a **CLI session** only if it is a `claude`
executable (or `node`/`bun` running `@anthropic-ai/claude-code`) that does **not** live in Desktop's data folder / the
`WindowsApps` / MSIX package folders and does **not** have a Claude Desktop process among its ancestors. So the Code tab of
Desktop (which runs its own `claude.exe` from `<Desktop data>\claude-code\`) never blocks a CLI switch, and a CLI in a
terminal never blocks a Desktop switch. IDE extensions that use the Claude Code binary (they share `~/.claude`) do count
as CLI sessions.

## What is backed up / switched
**Desktop** – `%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude` (MSIX; falls back to `%APPDATA%\Claude`).
Both modes are "whole folder minus excludes", so new files are picked up automatically.

| | Account slots (switch) | Backups |
|---|---|---|
| Sign-in (`config.json`, `Local State`, `Network/`, storage, …) | yes | yes |
| MCP config `claude_desktop_config.json`, `Claude Extensions` | yes | yes |
| Sessions (`local-agent-mode-sessions`, `claude-code-sessions`, uploads) | no (left as they are, see note) | yes |
| Git data (`git-shadow`, `git-worktrees.json`, `scratch-workspaces`) | no | only if *Include git data* is on |
| Caches, logs, `vm_bundles` / Cowork VM, `claude-code` binaries, lock files | no | no |

**CLI** – `~/.claude` plus the `oauthAccount` and `mcpServers` keys of `~/.claude.json` (the rest of that file is never
touched).

| | Account slots (switch) | Backups |
|---|---|---|
| Login (`.credentials.json`, `oauthAccount`) | yes | yes |
| `settings.json`, `skills/`, `commands/`, `agents/`, `plugins/`, `CLAUDE.md`, `mcpServers` | yes | yes |
| Sessions (`projects/`, `todos/`, `plans/`, `history.jsonl`) | no | yes, unless *Include CLI sessions* is off |
| Caches, `statsig`, `debug`, `shell-snapshots`, `ide`, … | no | no |

**Why sessions are not part of an account slot:** Claude already keeps them apart per account. Inside
`local-agent-mode-sessions` and `claude-code-sessions` the folders are `<account id>/<organization id>/…`, and Claude only
shows the sessions of the account that is signed in (verified on a real work + personal pair: each account sees only its
own sessions after a switch). So the switch only has to change the identity files (`config.json`, `Local State`,
`cowork-enabled-cli-ops.json`, `Network/`, storage); both accounts' sessions stay on disk side by side, and backups
include all of them.

Lists: `DESKTOP_EXCLUDE`, `DESKTOP_BACKUP_EXCLUDE`, `DESKTOP_GIT_ITEMS`, `CODE_EXCLUDE`, `CODE_BACKUP_EXCLUDE`,
`CODE_SESSION_ITEMS` in `core/src/lib.rs`.
The Cowork VM needs nothing special: only the one default Claude data folder is ever used (no extra profiles).

**Restore**: restoring a backup puts back *everything inside that archive* (whatever the exclude / git settings are now)
and never deletes live data that is not in it. Switch / rollback / recover only touch the switch scope.

## Where things are stored
`%LOCALAPPDATA%\claude-swap\`
```
accounts/desktop/<name>/…                      Desktop account snapshots (plain folders)
accounts/code/<name>/…                         CLI account snapshots
backups/<account>/<ms>-<instance>-<reason>.tar.gz  backups; <instance> = desktop | code;
                                               <account> = that instance's active account, else "<email>_unassigned" / "_unassigned"
settings.json   state.json   pending.json   pending-code.json
```
Every slot's `meta.json` and every backup archive's `meta.json` carry `"instance": "desktop" | "code"`; the backup file
name carries it too, so backups can be listed per instance without unpacking them. `state.json` is
`{ "active": { "desktop": "<name>", "code": "<name>" } }`.

The newest N backups are kept per account folder **and instance**: `Swapper::with_max_backups(Some(n))` /
`detect_with(Some(n))` > `max_backups` in `settings.json` > default 15.

**Upgrading from an older version:** on start, `accounts/<name>` is moved to `accounts/desktop/<name>`, a `state.json` with a
single `"active": "<name>"` is read as the Desktop entry, `pending.json` stays the Desktop marker, and backups named
`<ms>-<reason>.tar.gz` count as Desktop backups. Nothing needs to be redone. The `manage_desktop` / `manage_code` settings
are gone (both instances are always available) and are ignored if still present in `settings.json`.

## Safety
The instance you operate on must be closed for a switch: Desktop (the app can close it for you) or all CLI sessions
(you close them). The other instance may keep running. Every overwrite is preceded by a backup; a failed switch is
rolled back from it.

## Build / run (Windows 10/11)
```
cargo test -p swap-core                  # logic tests (these also run on other OSes)
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
- [ ] GUI to list backups (with their Desktop / CLI tag) and *Restore* one (`Swapper::list_backups` / `restore_backup`
      exist; the tag is in `BackupEntry::instance`).
- [ ] Verify on Windows: live backup with locked files, the Open Claude launch (AUMID lookup), whether Cowork sessions are per account.
- [ ] Optional: store account slots as `.tar.gz` too.

## License
Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this
project, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
