#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! claude-swap GUI (iced). Contains no backup/switch logic: every action calls `swap_core::Swapper`
//! on a background thread. Scope today: Claude Desktop only (Claude Code support is a README TODO).

use iced::widget::{button, column, container, row, scrollable, text, text_input, toggler, Container, Space};
use iced::{time, Element, Length, Subscription, Task, Theme};
use std::time::Duration;
use swap_core::{LargeItem, Status, Swapper};

fn main() -> iced::Result {
    iced::application(App::new, App::update, App::view)
        .title("Claude Swap")
        .subscription(App::subscription)
        .theme(|_: &App| Theme::Dark)
        .window_size((600.0, 720.0))
        .run()
}

/// Run blocking core code off the UI thread.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

fn sw() -> Result<Swapper, String> {
    Swapper::detect().map_err(e2s)
}

fn e2s(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[derive(Debug, Clone)]
enum Confirm {
    Delete(String),
    NewAccount,
    Overwrite(String),
    Recover,
}

struct App {
    status: Option<Status>,
    /// (is_ok, text)
    msg: Option<(bool, String)>,
    busy: bool,
    name: String,
    rename: Option<(String, String)>,
    confirm: Option<Confirm>,
    large: Vec<LargeItem>,
    tab: Tab,
    max_input: String,
    thr_input: String,
    settings_loaded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Accounts,
    Settings,
}

#[derive(Debug, Clone)]
enum Msg {
    Tick,
    Status(Result<Status, String>),
    Done(Result<String, String>),
    NameChanged(String),
    SaveClicked,
    Switch(String),
    Ask(Confirm),
    Yes,
    No,
    NewAccount,
    CloseClaude,
    Launch,
    RenameStart(String),
    RenameInput(String),
    RenameOk,
    RenameCancel,
    Large(Result<Vec<LargeItem>, String>),
    ExcludeItem(String, String),
    KeepItem(String, String),
    SetTab(Tab),
    MaxInput(String),
    ThrInput(String),
    SaveSettings,
    BackupNow,
    OpenStore,
    OpenBackups,
    Unexclude(String),
    Unkeep(String),
    ToggleGit(bool),
}

/// Short-lived notices: dropped as soon as the status poll shows they are no longer news
/// (Claude is up after a launch / gone after a close), so they do not linger on screen.
const LAUNCHING: &str = "Launching Claude…";
const CLOSED: &str = "Claude closed";

fn refresh() -> Task<Msg> {
    Task::perform(blocking(|| Ok(sw()?.status())), Msg::Status)
}

/// Large-item scan. Walks the disk, so only on start-up and after actions – not on every tick.
fn scan_large() -> Task<Msg> {
    Task::perform(blocking(|| Ok(sw()?.scan_large())), Msg::Large)
}

fn human(b: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let (mut v, mut i) = (b as f64, 0);
    while v >= 1024.0 && i < 3 { v /= 1024.0; i += 1; }
    format!("{v:.1} {}", U[i])
}

fn run(label: &'static str, f: impl FnOnce() -> Result<(), String> + Send + 'static) -> Task<Msg> {
    Task::perform(blocking(move || f().map(|_| label.to_string())), Msg::Done)
}

fn card<'a>(c: Element<'a, Msg>) -> Container<'a, Msg> {
    container(c).padding(12).style(container::rounded_box).width(Length::Fill)
}

fn run_msg(f: impl FnOnce() -> Result<String, String> + Send + 'static) -> Task<Msg> {
    Task::perform(blocking(f), Msg::Done)
}

impl App {
    fn new() -> (App, Task<Msg>) {
        (
            App { status: None, msg: None, busy: false, name: String::new(), rename: None, confirm: None, large: vec![], tab: Tab::Accounts, max_input: String::new(), thr_input: String::new(), settings_loaded: false },
            Task::batch([refresh(), scan_large()]),
        )
    }

    fn subscription(&self) -> Subscription<Msg> {
        time::every(Duration::from_secs(4)).map(|_| Msg::Tick)
    }

    fn start(&mut self, t: Task<Msg>) -> Task<Msg> {
        self.busy = true;
        self.msg = Some((true, "Working…".into()));
        t
    }

    fn running(&self) -> bool { self.status.as_ref().map(|s| s.claude_running).unwrap_or(false) }

    fn update(&mut self, m: Msg) -> Task<Msg> {
        match m {
            Msg::Tick => if self.busy { Task::none() } else { refresh() },
            Msg::Status(r) => {
                match r {
                    Ok(s) => {
                        if !self.settings_loaded {
                            self.max_input = s.settings.max_backups.map(|n| n.to_string()).unwrap_or_default();
                            self.thr_input = s.settings.threshold_mb.to_string();
                            self.settings_loaded = true;
                        }
                        let transient = match self.msg.as_ref().map(|(ok, t)| (*ok, t.as_str())) {
                            Some((true, LAUNCHING)) => s.claude_running,
                            Some((true, CLOSED)) => !s.claude_running,
                            _ => false,
                        };
                        if transient { self.msg = None; }
                        self.status = Some(s);
                    }
                    Err(e) => self.msg = Some((false, e)),
                }
                Task::none()
            }
            Msg::Done(r) => {
                self.busy = false;
                self.msg = Some(match r { Ok(s) => (true, s), Err(e) => (false, e) });
                Task::batch([refresh(), scan_large()])
            }
            Msg::NameChanged(s) => { self.name = s; Task::none() }
            Msg::SaveClicked => {
                let n = self.name.trim().to_string();
                if n.is_empty() { self.msg = Some((false, "Enter a name first".into())); return Task::none(); }
                let exists = self.status.as_ref().map(|s| s.accounts.iter().any(|a| a.name == n)).unwrap_or(false);
                if exists { self.confirm = Some(Confirm::Overwrite(n)); return Task::none(); }
                self.save(n, false)
            }
            Msg::Switch(n) => {
                let close = self.running();
                let _ = close; // switching requires Claude closed; button is disabled while running
                let t = run("Switched – Claude is starting", move || {
                    sw()?.switch_and_launch(&n, false).map_err(e2s)
                });
                self.start(t)
            }
            Msg::Ask(c) => { self.confirm = Some(c); Task::none() }
            Msg::No => { self.confirm = None; Task::none() }
            Msg::Yes => {
                let Some(c) = self.confirm.take() else { return Task::none() };
                match c {
                    Confirm::Delete(n) => {
                        let t = run("Deleted", move || sw()?.delete_account(&n).map_err(e2s));
                        self.start(t)
                    }
                    Confirm::Overwrite(n) => self.save(n, true),
                    Confirm::NewAccount => {
                        let t = run("Sign in to the new account in the Claude window, close Claude, then press “Save current login”.", || {
                            sw()?.start_new_account(true).map_err(e2s)
                        });
                        self.start(t)
                    }
                    Confirm::Recover => {
                        let t = run("Previous state restored", || sw()?.recover_closing(true).map(|_| ()).map_err(e2s));
                        self.start(t)
                    }
                }
            }
            Msg::NewAccount => { self.confirm = Some(Confirm::NewAccount); Task::none() }
            Msg::CloseClaude => {
                let t = run(CLOSED, || swap_core::kill_desktop().map(|_| ()).map_err(e2s));
                self.start(t)
            }
            Msg::Launch => {
                let t = run(LAUNCHING, || {
                    let s = sw()?;
                    swap_core::launch_desktop(&s.paths.desktop).map_err(e2s)
                });
                self.start(t)
            }
            Msg::Large(r) => {
                if let Ok(l) = r { self.large = l; }
                Task::none()
            }
            Msg::ExcludeItem(root, name) => {
                let t = run("Excluded – it will no longer be backed up or swapped", move || sw()?.exclude_item(&root, &name).map_err(e2s));
                self.start(t)
            }
            Msg::KeepItem(root, name) => {
                let t = run("Kept – no more warnings for it", move || sw()?.acknowledge_item(&root, &name).map_err(e2s));
                self.start(t)
            }
            Msg::SetTab(t) => { self.tab = t; Task::none() }
            Msg::MaxInput(v) => { self.max_input = v.chars().filter(|c| c.is_ascii_digit()).collect(); Task::none() }
            Msg::ThrInput(v) => { self.thr_input = v.chars().filter(|c| c.is_ascii_digit()).collect(); Task::none() }
            Msg::SaveSettings => {
                let max = if self.max_input.trim().is_empty() { None } else { self.max_input.trim().parse::<usize>().ok() };
                if !self.max_input.trim().is_empty() && max.is_none() {
                    self.msg = Some((false, "Max backups must be a number (or empty for the default)".into()));
                    return Task::none();
                }
                let Ok(thr) = self.thr_input.trim().parse::<u64>() else {
                    self.msg = Some((false, "Large-item threshold must be a number of MB".into()));
                    return Task::none();
                };
                self.settings_loaded = false; // re-sync the inputs from disk afterwards
                let t = run("Settings saved", move || {
                    let s = sw()?;
                    s.set_max_backups(max).map_err(e2s)?;
                    s.set_threshold_mb(thr).map_err(e2s)
                });
                self.start(t)
            }
            Msg::BackupNow => {
                let t = run_msg(|| {
                    let r = sw()?.backup_now().map_err(e2s)?;
                    let mut m = format!("Backup saved ({})", human(r.size_bytes));
                    if r.live {
                        m.push_str(" – Claude was running, so this is a best-effort backup");
                        if !r.skipped.is_empty() { m.push_str(&format!("; {} file(s) were locked and skipped", r.skipped.len())); }
                    }
                    Ok(m)
                });
                self.start(t)
            }
            Msg::OpenStore => {
                match sw().and_then(|s| s.open_store_dir().map_err(e2s)) {
                    Ok(()) => {}
                    Err(e) => self.msg = Some((false, e)),
                }
                Task::none()
            }
            Msg::OpenBackups => {
                match sw().and_then(|s| s.open_backups_dir().map_err(e2s)) {
                    Ok(()) => {}
                    Err(e) => self.msg = Some((false, e)),
                }
                Task::none()
            }
            Msg::Unexclude(k) => {
                let t = run("Included again", move || sw()?.remove_exclude(&k).map_err(e2s));
                self.start(t)
            }
            Msg::ToggleGit(on) => {
                let t = run(if on { "Git data is now included in backups" } else { "Git data is no longer included in backups" },
                    move || sw()?.set_backup_git(on).map_err(e2s));
                self.start(t)
            }
            Msg::Unkeep(k) => {
                let t = run("Will warn about it again if it is large", move || sw()?.forget_acknowledged(&k).map_err(e2s));
                self.start(t)
            }
            Msg::RenameStart(n) => { self.rename = Some((n.clone(), n)); Task::none() }
            Msg::RenameInput(s) => { if let Some((_, v)) = self.rename.as_mut() { *v = s; } Task::none() }
            Msg::RenameCancel => { self.rename = None; Task::none() }
            Msg::RenameOk => {
                let Some((old, new)) = self.rename.take() else { return Task::none() };
                let new = new.trim().to_string();
                if new == old || new.is_empty() { return Task::none(); }
                let t = run("Renamed", move || sw()?.rename_account(&old, &new).map_err(e2s));
                self.start(t)
            }
        }
    }

    fn save(&mut self, name: String, overwrite: bool) -> Task<Msg> {
        self.name.clear();
        let t = run("Saved", move || sw()?.save_login(&name, overwrite).map_err(e2s));
        self.start(t)
    }

    fn settings_view(&self) -> Element<'_, Msg> {
        let mut col = column![].spacing(12);

        col = col.push(card(column![
            text("What is backed up and switched").size(16),
            text("Claude Desktop: the app data folder – sign-in, MCP config (claude_desktop_config.json), extensions. Backups also hold Cowork / Desktop sessions.").size(13),
            text("Claude Code (CLI, ~/.claude): not handled yet – planned. Nothing under ~/.claude is read, copied or changed.").size(12),
        ].spacing(6).into()));

        let eff = self.status.as_ref().map(|s| s.max_backups).unwrap_or(0);
        col = col.push(card(column![
            text("Backups").size(16),
            row![
                text("Max backups kept per account").width(Length::Fill),
                text_input("default", &self.max_input).on_input(Msg::MaxInput).width(110),
            ].spacing(8).align_y(iced::Center),
            text(format!("Empty = built-in default. Currently keeping {eff} per account.")).size(12),
            toggler(self.status.as_ref().map(|s| s.settings.backup_git).unwrap_or(false))
                .label("Include git data in backups (git-shadow, git-worktrees.json, scratch-workspaces)")
                .on_toggle(Msg::ToggleGit),
            text("Off by default because these can be large. Backups never restore less than what is inside them.").size(12),
            row![
                text("Warn when a folder/file is bigger than (MB)").width(Length::Fill),
                text_input("100", &self.thr_input).on_input(Msg::ThrInput).width(110),
            ].spacing(8).align_y(iced::Center),
            button("Save settings").style(button::primary).on_press(Msg::SaveSettings),
        ].spacing(8).into()));

        let (store, backups) = self.status.as_ref().map(|s| (s.store_dir.clone(), s.backups_dir.clone())).unwrap_or_default();
        col = col.push(card(column![
            text("Folders").size(16),
            row![
                column![text("App folder").size(14), text(store).size(11)].width(Length::Fill),
                button("Open").style(button::secondary).on_press(Msg::OpenStore),
            ].spacing(8).align_y(iced::Center),
            row![
                column![text("Backups folder").size(14), text(backups).size(11)].width(Length::Fill),
                button("Open").style(button::secondary).on_press(Msg::OpenBackups),
            ].spacing(8).align_y(iced::Center),
        ].spacing(8).into()));

        if let Some(s) = &self.status {
            let mut lists = column![text("Excluded / kept items").size(16)].spacing(6);
            if s.settings.exclude.is_empty() && s.settings.acknowledged.is_empty() {
                lists = lists.push(text("Nothing yet. Large items you exclude or keep from the warning banner show up here.").size(12));
            }
            for k in &s.settings.exclude {
                lists = lists.push(row![
                    text(format!("Excluded: {k}")).size(13).width(Length::Fill),
                    button("Include again").style(button::secondary).on_press(Msg::Unexclude(k.clone())),
                ].spacing(8).align_y(iced::Center));
            }
            for k in &s.settings.acknowledged {
                lists = lists.push(row![
                    text(format!("Kept (no warning): {k}")).size(13).width(Length::Fill),
                    button("Warn again").style(button::secondary).on_press(Msg::Unkeep(k.clone())),
                ].spacing(8).align_y(iced::Center));
            }
            col = col.push(card(lists.into()));
        }
        scrollable(col).height(Length::Fill).into()
    }

    fn view(&self) -> Element<'_, Msg> {
        let running = self.running();
        let mut page = column![].spacing(12).padding(16);

        let tab_btn = |label: &'static str, t: Tab| {
            button(label).style(if self.tab == t { button::primary } else { button::secondary }).on_press(Msg::SetTab(t))
        };
        page = page.push(
            row![
                text("Claude Swap").size(24),
                Space::new().width(Length::Fill),
                tab_btn("Accounts", Tab::Accounts),
                tab_btn("Settings", Tab::Settings),
            ].spacing(8).align_y(iced::Center),
        );
        page = page.push(text("One Claude account at a time – Claude Desktop").size(13));

        // --- Claude status card
        let status_line = match &self.status {
            None => "Loading…".to_string(),
            Some(s) => format!(
                "Claude Desktop is {}{}{}",
                if s.claude_running { "running" } else { "closed" },
                s.live_email.as_ref().map(|m| format!("   ·   Signed in: {m}")).unwrap_or_default(),
                s.active.as_ref().map(|a| format!("   ·   Active: {a}")).unwrap_or_default()
            ),
        };
        let mut close_btn = button("Close Claude").style(button::secondary);
        if running && !self.busy { close_btn = close_btn.on_press(Msg::CloseClaude); }
        let mut open_btn = button("Open Claude").style(button::secondary);
        if !self.busy { open_btn = open_btn.on_press(Msg::Launch); }
        let mut bk_btn = button("Back up now").style(button::secondary);
        if !self.busy { bk_btn = bk_btn.on_press(Msg::BackupNow); }
        page = page.push(
            container(row![text(status_line).size(13).width(Length::Fill), bk_btn, close_btn, open_btn].spacing(8).align_y(iced::Center))
                .padding(12).style(container::rounded_box).width(Length::Fill),
        );

        // --- message
        if let Some((ok, t)) = &self.msg {
            let c = if *ok { iced::Color::from_rgb(0.42, 0.75, 0.55) } else { iced::Color::from_rgb(0.95, 0.53, 0.49) };
            page = page.push(text(t.clone()).size(13).color(c));
        }

        // --- large item alerts (new big things in Claude's folders that would go into every backup)
        for it in &self.large {
            let mut ex = button("Exclude").style(button::danger);
            let mut keep = button("Keep").style(button::secondary);
            if !self.busy {
                ex = ex.on_press(Msg::ExcludeItem(it.root.clone(), it.name.clone()));
                keep = keep.on_press(Msg::KeepItem(it.root.clone(), it.name.clone()));
            }
            page = page.push(
                container(row![
                    text(format!("Large item: {}/{} is {} and is included in every backup and account snapshot.", it.root, it.name, human(it.bytes)))
                        .size(13).width(Length::Fill),
                    ex, keep,
                ].spacing(8).align_y(iced::Center))
                .padding(12).style(container::bordered_box).width(Length::Fill),
            );
        }

        // --- confirmation banner
        if let Some(c) = &self.confirm {
            let q = match c {
                Confirm::Delete(n) => format!("Delete saved account “{n}”? (the live login is not touched)"),
                Confirm::Overwrite(n) => format!("“{n}” already exists. Overwrite it with the current login?"),
                Confirm::NewAccount => "Store the current account, sign Claude out and open it so you can log in to another account?".to_string(),
                Confirm::Recover => "Undo the interrupted switch / add-account and restore the previous state?".to_string(),
            };
            page = page.push(
                container(row![
                    text(q).size(13).width(Length::Fill),
                    button("Yes").style(button::primary).on_press(Msg::Yes),
                    button("Cancel").style(button::secondary).on_press(Msg::No),
                ].spacing(8).align_y(iced::Center))
                .padding(12).style(container::bordered_box).width(Length::Fill),
            );
        }

        // --- recovery
        if self.status.as_ref().map(|s| s.pending_recovery).unwrap_or(false) && self.confirm.is_none() {
            page = page.push(
                container(row![
                    text("An add-account / switch was interrupted. You can undo it.").size(13).width(Length::Fill),
                    button("Recover previous state").style(button::secondary).on_press(Msg::Ask(Confirm::Recover)),
                ].spacing(8).align_y(iced::Center))
                .padding(12).style(container::rounded_box).width(Length::Fill),
            );
        }

        if self.tab == Tab::Settings {
            page = page.push(self.settings_view());
            return page.into();
        }

        // --- accounts
        let mut add = button("+ Add new account").style(button::primary);
        if !self.busy { add = add.on_press(Msg::NewAccount); }
        page = page.push(row![text("Accounts").size(16).width(Length::Fill), add].align_y(iced::Center));

        let mut list = column![].spacing(8);
        match &self.status {
            Some(s) if !s.accounts.is_empty() => {
                for a in &s.accounts {
                    let cur = s.active.as_deref() == Some(a.name.as_str());
                    if let Some((old, val)) = &self.rename {
                        if old == &a.name {
                            list = list.push(
                                container(row![
                                    text_input("new name", val).on_input(Msg::RenameInput).on_submit(Msg::RenameOk).width(Length::Fill),
                                    button("OK").style(button::primary).on_press(Msg::RenameOk),
                                    button("Cancel").style(button::secondary).on_press(Msg::RenameCancel),
                                ].spacing(8).align_y(iced::Center))
                                .padding(12).style(container::rounded_box).width(Length::Fill),
                            );
                            continue;
                        }
                    }
                    let title = if cur { format!("{}  (active)", a.name) } else { a.name.clone() };
                    let mut sw_btn = button(if cur { "Reload" } else { "Switch" })
                        .style(if cur { button::secondary } else { button::primary });
                    if !running && !self.busy { sw_btn = sw_btn.on_press(Msg::Switch(a.name.clone())); }
                    let mut rn = button("Rename").style(button::secondary);
                    let mut del = button("Delete").style(button::danger);
                    if !self.busy {
                        rn = rn.on_press(Msg::RenameStart(a.name.clone()));
                        del = del.on_press(Msg::Ask(Confirm::Delete(a.name.clone())));
                    }
                    list = list.push(
                        container(row![
                            column![text(title).size(15), text(a.email.clone().unwrap_or_default()).size(12)].width(Length::Fill),
                            sw_btn, rn, del,
                        ].spacing(8).align_y(iced::Center))
                        .padding(12).style(container::rounded_box).width(Length::Fill),
                    );
                }
            }
            Some(_) => {
                list = list.push(text("No saved accounts yet. Sign in to Claude, close it, then save the login below.").size(13));
            }
            None => {}
        }
        page = page.push(scrollable(list).height(Length::Fill));

        // --- save current login
        let can_save = !running && !self.busy && self.status.as_ref().map(|s| s.desktop_signed_in).unwrap_or(false);
        let mut save = button("Save current login").style(button::primary);
        if can_save { save = save.on_press(Msg::SaveClicked); }
        let mut input = text_input("Name for the account Claude is signed in to (e.g. Work)", &self.name).width(Length::Fill);
        if can_save { input = input.on_input(Msg::NameChanged).on_submit(Msg::SaveClicked); }
        page = page.push(
            container(column![
                text("Signed in to an account that isn't saved yet? Close Claude and give it a name:").size(13),
                row![input, save].spacing(8),
            ].spacing(8)).padding(12).style(container::rounded_box).width(Length::Fill),
        );

        // --- paths
        if let Some(s) = &self.status {
            page = page.push(text(format!("Data: {}\nStore: {}", s.desktop_dir, s.store_dir)).size(11));
        }
        page = page.push(Space::new().height(0));
        page.into()
    }
}
