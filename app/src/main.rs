#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! claude-swap GUI (iced). Contains no backup/switch logic: every action calls `swap_core::Swapper`
//! on a background thread. Claude Desktop and the Claude Code CLI are handled as two separate instances
//! (each with its own accounts, active account and backups) and are tagged "Desktop" / "CLI" everywhere.

use iced::widget::{button, column, container, row, scrollable, text, text_input, toggler, Container, Space};
use iced::{time, Background, Border, Color, Element, Length, Subscription, Task, Theme};
use std::time::Duration;
use swap_core::{Instance, InstanceStatus, LargeItem, Meta, Status, Swapper};

fn main() -> iced::Result {
    iced::application(App::new, App::update, App::view)
        .title("Claude Swap")
        .subscription(App::subscription)
        .theme(|_: &App| Theme::Dark)
        .window(iced::window::Settings {
            size: iced::Size::new(640.0, 760.0),
            icon: iced::window::icon::from_rgba(include_bytes!("../assets/icon-128.rgba").to_vec(), 128, 128).ok(),
            ..Default::default()
        })
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
    Delete(Instance, String),
    NewAccount(Instance),
    Overwrite(Instance, String),
    Recover(Instance),
}

struct App {
    status: Option<Status>,
    /// (is_ok, text)
    msg: Option<(bool, String)>,
    busy: bool,
    name: String,
    /// which instance "Save current login" / "+ Add new account" act on
    target: Instance,
    /// (instance, old name, new name)
    rename: Option<(Instance, String, String)>,
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
    SetTarget(Instance),
    SaveClicked,
    Switch(Instance, String),
    Ask(Confirm),
    Yes,
    No,
    NewAccount,
    CloseClaude,
    Launch,
    RenameStart(Instance, String),
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
    BackupNow(Instance),
    OpenStore,
    OpenBackups,
    Unexclude(String),
    Unkeep(String),
    ToggleGit(bool),
    ToggleCodeSessions(bool),
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

/// Colored "Desktop" / "CLI" tag.
fn badge<'a>(i: Instance) -> Element<'a, Msg> {
    let c = match i {
        Instance::Desktop => Color::from_rgb(0.45, 0.65, 1.0),
        Instance::Code => Color::from_rgb(1.0, 0.70, 0.30),
    };
    container(text(i.label()).size(11).color(c))
        .padding([2, 7])
        .style(move |_: &Theme| container::Style {
            background: Some(Background::Color(Color { a: 0.16, ..c })),
            border: Border { color: Color { a: 0.55, ..c }, width: 1.0, radius: 4.0.into() },
            ..Default::default()
        })
        .into()
}

impl App {
    fn new() -> (App, Task<Msg>) {
        (
            App {
                status: None, msg: None, busy: false, name: String::new(), target: Instance::Desktop, rename: None,
                confirm: None, large: vec![], tab: Tab::Accounts, max_input: String::new(), thr_input: String::new(),
                settings_loaded: false,
            },
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

    fn running(&self, i: Instance) -> bool { self.status.as_ref().map(|s| s.get(i).running).unwrap_or(false) }

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
                            Some((true, LAUNCHING)) => s.desktop.running,
                            Some((true, CLOSED)) => !s.desktop.running,
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
            Msg::SetTarget(i) => { self.target = i; Task::none() }
            Msg::SaveClicked => {
                let n = self.name.trim().to_string();
                if n.is_empty() { self.msg = Some((false, "Enter a name first".into())); return Task::none(); }
                let i = self.target;
                let exists = self.status.as_ref().map(|s| s.get(i).accounts.iter().any(|a| a.name == n)).unwrap_or(false);
                if exists { self.confirm = Some(Confirm::Overwrite(i, n)); return Task::none(); }
                self.save(i, n, false)
            }
            Msg::Switch(i, n) => {
                let t = run(
                    match i {
                        Instance::Desktop => "Switched – Claude is starting",
                        Instance::Code => "Switched the CLI account – new `claude` sessions use it",
                    },
                    move || sw()?.switch_and_launch(i, &n, false).map_err(e2s),
                );
                self.start(t)
            }
            Msg::Ask(c) => { self.confirm = Some(c); Task::none() }
            Msg::No => { self.confirm = None; Task::none() }
            Msg::Yes => {
                let Some(c) = self.confirm.take() else { return Task::none() };
                match c {
                    Confirm::Delete(i, n) => {
                        let t = run("Deleted", move || sw()?.delete_account(i, &n).map_err(e2s));
                        self.start(t)
                    }
                    Confirm::Overwrite(i, n) => self.save(i, n, true),
                    Confirm::NewAccount(i) => {
                        let t = run(
                            match i {
                                Instance::Desktop => "Sign in to the new account in the Claude window, close Claude, then press “Save current login”.",
                                Instance::Code => "Signed out of the CLI. Run `claude` and log in with the new account, close all CLI sessions, then press “Save current login”.",
                            },
                            move || sw()?.start_new_account(i, true).map_err(e2s),
                        );
                        self.start(t)
                    }
                    Confirm::Recover(i) => {
                        let t = run("Previous state restored", move || sw()?.recover_closing(i, true).map(|_| ()).map_err(e2s));
                        self.start(t)
                    }
                }
            }
            Msg::NewAccount => { self.confirm = Some(Confirm::NewAccount(self.target)); Task::none() }
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
            Msg::BackupNow(i) => {
                let t = run_msg(move || {
                    let r = sw()?.backup_now(i).map_err(e2s)?;
                    let mut m = format!("{} backup saved ({})", i.label(), human(r.size_bytes));
                    if r.live {
                        m.push_str(" – it was running, so this is a best-effort backup");
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
            Msg::ToggleCodeSessions(on) => {
                let t = run(if on { "CLI sessions are now included in backups" } else { "CLI sessions are no longer included in backups" },
                    move || sw()?.set_backup_code_sessions(on).map_err(e2s));
                self.start(t)
            }
            Msg::Unkeep(k) => {
                let t = run("Will warn about it again if it is large", move || sw()?.forget_acknowledged(&k).map_err(e2s));
                self.start(t)
            }
            Msg::RenameStart(i, n) => { self.rename = Some((i, n.clone(), n)); Task::none() }
            Msg::RenameInput(s) => { if let Some((_, _, v)) = self.rename.as_mut() { *v = s; } Task::none() }
            Msg::RenameCancel => { self.rename = None; Task::none() }
            Msg::RenameOk => {
                let Some((i, old, new)) = self.rename.take() else { return Task::none() };
                let new = new.trim().to_string();
                if new == old || new.is_empty() { return Task::none(); }
                let t = run("Renamed", move || sw()?.rename_account(i, &old, &new).map_err(e2s));
                self.start(t)
            }
        }
    }

    fn save(&mut self, i: Instance, name: String, overwrite: bool) -> Task<Msg> {
        self.name.clear();
        let t = run("Saved", move || sw()?.save_login(i, &name, overwrite).map_err(e2s));
        self.start(t)
    }

    fn settings_view(&self) -> Element<'_, Msg> {
        let mut col = column![].spacing(12);

        col = col.push(card(column![
            text("What is backed up and switched").size(16),
            row![badge(Instance::Desktop), text("the app data folder – sign-in, MCP config (claude_desktop_config.json), extensions. Backups also hold Cowork / Desktop sessions.").size(13)].spacing(8),
            row![badge(Instance::Code), text("~/.claude and the login/MCP keys of ~/.claude.json – credentials, settings, skills, commands, agents, plugins.").size(13)].spacing(8),
            text("Each one has its own accounts, active account and backups, and can run at the same time as the other.").size(12),
        ].spacing(6).into()));

        let eff = self.status.as_ref().map(|s| s.max_backups).unwrap_or(0);
        col = col.push(card(column![
            text("Backups").size(16),
            row![
                text("Max backups kept per account (for Desktop and CLI each)").width(Length::Fill),
                text_input("default", &self.max_input).on_input(Msg::MaxInput).width(110),
            ].spacing(8).align_y(iced::Center),
            text(format!("Empty = built-in default. Currently keeping {eff} per account.")).size(12),
            toggler(self.status.as_ref().map(|s| s.settings.backup_git).unwrap_or(false))
                .label("Desktop: include git data in backups (git-shadow, git-worktrees.json, scratch-workspaces)")
                .on_toggle(Msg::ToggleGit),
            text("Off by default because these can be large. Backups never restore less than what is inside them.").size(12),
            toggler(self.status.as_ref().map(|s| s.settings.backup_code_sessions).unwrap_or(true))
                .label("CLI: include sessions in backups (projects, todos, plans, history.jsonl)")
                .on_toggle(Msg::ToggleCodeSessions),
            text("On by default. Turn it off if the transcripts get too big; account switching never touches them.").size(12),
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

    /// One line of the status card: badge, state, signed-in e-mail, active account and the buttons.
    fn instance_row<'a>(&self, s: &'a InstanceStatus) -> Element<'a, Msg> {
        let i = s.instance;
        let line = format!(
            "{}{}{}",
            match (i, s.running) {
                (Instance::Desktop, true) => "Claude Desktop is running",
                (Instance::Desktop, false) => "Claude Desktop is closed",
                (Instance::Code, true) => "CLI session(s) running",
                (Instance::Code, false) => "No CLI session running",
            },
            s.live_email.as_ref().map(|m| format!("   ·   Signed in: {m}")).unwrap_or_default(),
            s.active.as_ref().map(|a| format!("   ·   Active: {a}")).unwrap_or_default(),
        );
        let mut bk = button("Back up now").style(button::secondary);
        if !self.busy { bk = bk.on_press(Msg::BackupNow(i)); }
        let mut r = row![badge(i), text(line).size(13).width(Length::Fill), bk].spacing(8).align_y(iced::Center);
        if i == Instance::Desktop {
            let mut close_btn = button("Close Claude").style(button::secondary);
            if s.running && !self.busy { close_btn = close_btn.on_press(Msg::CloseClaude); }
            let mut open_btn = button("Open Claude").style(button::secondary);
            if !self.busy { open_btn = open_btn.on_press(Msg::Launch); }
            r = r.push(close_btn).push(open_btn);
        }
        r.into()
    }

    fn account_card<'a>(&self, i: Instance, a: &'a Meta, active: Option<&str>) -> Element<'a, Msg> {
        if let Some((ri, old, val)) = &self.rename {
            if *ri == i && old == &a.name {
                return card(row![
                    badge(i),
                    text_input("new name", val).on_input(Msg::RenameInput).on_submit(Msg::RenameOk).width(Length::Fill),
                    button("OK").style(button::primary).on_press(Msg::RenameOk),
                    button("Cancel").style(button::secondary).on_press(Msg::RenameCancel),
                ].spacing(8).align_y(iced::Center).into()).into();
            }
        }
        let cur = active == Some(a.name.as_str());
        let title = if cur { format!("{}  (active)", a.name) } else { a.name.clone() };
        let mut sw_btn = button(if cur { "Reload" } else { "Switch" })
            .style(if cur { button::secondary } else { button::primary });
        if !self.running(i) && !self.busy { sw_btn = sw_btn.on_press(Msg::Switch(i, a.name.clone())); }
        let mut rn = button("Rename").style(button::secondary);
        let mut del = button("Delete").style(button::danger);
        if !self.busy {
            rn = rn.on_press(Msg::RenameStart(i, a.name.clone()));
            del = del.on_press(Msg::Ask(Confirm::Delete(i, a.name.clone())));
        }
        card(row![
            badge(i),
            column![text(title).size(15), text(a.email.clone().unwrap_or_default()).size(12)].width(Length::Fill),
            sw_btn, rn, del,
        ].spacing(8).align_y(iced::Center).into()).into()
    }

    fn view(&self) -> Element<'_, Msg> {
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
        page = page.push(text("One account at a time per instance – Claude Desktop and Claude Code CLI").size(13));

        // --- status card: one row per instance
        let mut status_col = column![].spacing(8);
        match &self.status {
            None => status_col = status_col.push(text("Loading…").size(13)),
            Some(s) => {
                for i in Instance::ALL { status_col = status_col.push(self.instance_row(s.get(i))); }
            }
        }
        page = page.push(card(status_col.into()));

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
            let tag = Instance::from_key(&it.root).map(badge);
            let mut r = row![].spacing(8).align_y(iced::Center);
            if let Some(t) = tag { r = r.push(t); }
            page = page.push(
                container(r
                    .push(text(format!("Large item: {}/{} is {} and is included in every backup and account snapshot.", it.root, it.name, human(it.bytes)))
                        .size(13).width(Length::Fill))
                    .push(ex).push(keep))
                .padding(12).style(container::bordered_box).width(Length::Fill),
            );
        }

        // --- confirmation banner
        if let Some(c) = &self.confirm {
            let q = match c {
                Confirm::Delete(i, n) => format!("Delete saved {} account “{n}”? (the live login is not touched)", i.label()),
                Confirm::Overwrite(i, n) => format!("{} account “{n}” already exists. Overwrite it with the current login?", i.label()),
                Confirm::NewAccount(Instance::Desktop) => "Store the current Desktop account, sign Claude out and open it so you can log in to another account?".to_string(),
                Confirm::NewAccount(Instance::Code) => "Store the current CLI account and sign the CLI out so you can log in to another account (run `claude` afterwards)? All CLI sessions must be closed.".to_string(),
                Confirm::Recover(i) => format!("Undo the interrupted {} switch / add-account and restore the previous state?", i.label()),
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
        if self.confirm.is_none() {
            if let Some(s) = &self.status {
                for i in Instance::ALL {
                    if !s.get(i).pending_recovery { continue; }
                    page = page.push(
                        container(row![
                            badge(i),
                            text("An add-account / switch was interrupted. You can undo it.").size(13).width(Length::Fill),
                            button("Recover previous state").style(button::secondary).on_press(Msg::Ask(Confirm::Recover(i))),
                        ].spacing(8).align_y(iced::Center))
                        .padding(12).style(container::rounded_box).width(Length::Fill),
                    );
                }
            }
        }

        if self.tab == Tab::Settings {
            page = page.push(self.settings_view());
            return page.into();
        }

        // --- accounts
        let target = self.target;
        let tgt_btn = |i: Instance| {
            button(i.label()).style(if target == i { button::primary } else { button::secondary }).on_press(Msg::SetTarget(i))
        };
        let mut add = button("+ Add new account").style(button::primary);
        if !self.busy { add = add.on_press(Msg::NewAccount); }
        page = page.push(
            row![
                text("Accounts").size(16).width(Length::Fill),
                text("For:").size(13),
                tgt_btn(Instance::Desktop),
                tgt_btn(Instance::Code),
                add,
            ].spacing(8).align_y(iced::Center),
        );

        let mut list = column![].spacing(8);
        match &self.status {
            Some(s) if !s.desktop.accounts.is_empty() || !s.code.accounts.is_empty() => {
                for i in Instance::ALL {
                    let st = s.get(i);
                    for a in &st.accounts {
                        list = list.push(self.account_card(i, a, st.active.as_deref()));
                    }
                }
            }
            Some(_) => {
                list = list.push(text("No saved accounts yet. Sign in to Claude (Desktop or CLI), close it, pick it with “For:” above and save the login below.").size(13));
            }
            None => {}
        }
        page = page.push(scrollable(list).height(Length::Fill));

        // --- save current login
        let can_save = !self.running(target) && !self.busy && self.status.as_ref().map(|s| s.get(target).signed_in).unwrap_or(false);
        let mut save = button("Save current login").style(button::primary);
        if can_save { save = save.on_press(Msg::SaveClicked); }
        let mut input = text_input(&format!("Name for the {} account that is signed in (e.g. Work)", target.label()), &self.name).width(Length::Fill);
        if can_save { input = input.on_input(Msg::NameChanged).on_submit(Msg::SaveClicked); }
        page = page.push(
            container(column![
                row![
                    badge(target),
                    text(match target {
                        Instance::Desktop => "Signed in to an account that isn't saved yet? Close Claude and give it a name:",
                        Instance::Code => "Signed in to an account that isn't saved yet? Close all CLI sessions and give it a name:",
                    }).size(13),
                ].spacing(8).align_y(iced::Center),
                row![input, save].spacing(8),
            ].spacing(8)).padding(12).style(container::rounded_box).width(Length::Fill),
        );

        // --- paths
        if let Some(s) = &self.status {
            page = page.push(text(format!("Desktop data: {}\nCLI data: {}\nStore: {}", s.desktop.dir, s.code.dir, s.store_dir)).size(11));
        }
        page = page.push(Space::new().height(0));
        page.into()
    }
}
