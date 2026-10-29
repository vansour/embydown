//! 交互式终端界面：直接运行 `embydown`（或 `embydown ui`）进入。
//!
//! 覆盖常用功能：多服务器管理（登录/删除）、媒体库浏览、跨服务器搜索、
//! 条目详情、版本与季选择、直链查看、下载参数设置与下载。

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Tabs,
    Wrap,
};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc;

use crate::config::{self, Config, Profile, Settings};
use crate::download::DownloadOpts;
use crate::manager::{FinishedStatus, Manager};
use crate::emby::Client;
use crate::model::Item;
use crate::util::{format_bytes, parse_rate};
use crate::{
    expand_versions, filter_to_version, has_multiple_versions, resolve_item, version_summaries,
    VersionSummary,
};
use unicode_width::UnicodeWidthStr;

const SEARCH_LIMIT: u32 = 30;
const DIRECT_SERVER: &str = "（直链）";

pub async fn run() -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("交互界面需要在终端中运行；脚本请使用子命令，例如 embydown dl <关键字> --dry-run");
    }

    let cfg = config::load_optional()?.unwrap_or_default();
    let mut clients: BTreeMap<String, Client> = BTreeMap::new();
    let mut connect_errors = Vec::new();
    for (name, profile) in cfg.servers.iter() {
        match crate::client_for(name, profile).await {
            Ok(client) => {
                clients.insert(name.clone(), client);
            }
            Err(e) => connect_errors.push(format!("{name}: {e:#}")),
        }
    }

    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();

    // 独立线程读终端事件，主循环用 tokio::select! 同时处理事件与后台任务结果。
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if ev_tx.send(ev).is_err() {
                break;
            }
        }
    });

    let mut app = App::new(cfg, clients, msg_tx);
    if !connect_errors.is_empty() {
        app.set_error(format!("部分服务器连接失败：{}", connect_errors.join("；")));
    } else if app.cfg.is_empty() {
        app.set_info("还没有登录服务器：按 3 到「服务器」页，按 a 添加");
    }

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, msg_rx, ev_rx).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    mut msg_rx: mpsc::UnboundedReceiver<Msg>,
    mut ev_rx: mpsc::UnboundedReceiver<Event>,
) -> Result<()> {
    loop {
        terminal.draw(|f| app.draw(f))?;
        let tick = tokio::time::sleep(Duration::from_millis(150));
        tokio::pin!(tick);

        tokio::select! {
            ev = ev_rx.recv() => {
                let Some(ev) = ev else { break };
                if let Event::Key(key) = ev {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    app.on_key(key)?;
                }
            }
            msg = msg_rx.recv() => {
                if let Some(msg) = msg {
                    app.on_msg(msg);
                }
            }
            _ = &mut tick => app.tick(),
        }
        if app.quit {
            break;
        }
    }
    Ok(())
}

// ───────────────────────── 数据结构 ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Search,
    Browse,
    Downloads,
    Servers,
    Settings,
}

impl Tab {
    fn index(self) -> usize {
        match self {
            Tab::Search => 0,
            Tab::Browse => 1,
            Tab::Downloads => 2,
            Tab::Servers => 3,
            Tab::Settings => 4,
        }
    }

    fn next(self) -> Tab {
        match self {
            Tab::Search => Tab::Browse,
            Tab::Browse => Tab::Downloads,
            Tab::Downloads => Tab::Servers,
            Tab::Servers => Tab::Settings,
            Tab::Settings => Tab::Search,
        }
    }

    fn prev(self) -> Tab {
        match self {
            Tab::Search => Tab::Settings,
            Tab::Browse => Tab::Search,
            Tab::Downloads => Tab::Browse,
            Tab::Servers => Tab::Downloads,
            Tab::Settings => Tab::Servers,
        }
    }

    fn from_digit(c: char) -> Option<Tab> {
        match c {
            '1' => Some(Tab::Search),
            '2' => Some(Tab::Browse),
            '3' => Some(Tab::Downloads),
            '4' => Some(Tab::Servers),
            '5' => Some(Tab::Settings),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Info,
    Ok,
    Warn,
    Err,
}

#[derive(Clone)]
struct SearchHit {
    server: String,
    item: Item,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchFocus {
    Query,
    Results,
}

struct SearchState {
    query: TextInput,
    focus: SearchFocus,
    server: Option<String>,
    hits: Vec<SearchHit>,
    state: TableState,
    pending: Option<u64>,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            query: TextInput::default(),
            focus: SearchFocus::Results,
            server: None,
            hits: Vec::new(),
            state: TableState::default(),
            pending: None,
        }
    }
}

struct BrowseLevel {
    server: String,
    parent: Option<String>,
    title: String,
    items: Vec<Item>,
}

#[derive(Default)]
struct BrowseState {
    levels: Vec<BrowseLevel>,
    state: ListState,
    server_state: ListState,
    pending: Option<u64>,
}

#[derive(Clone)]
enum VersionChoice {
    Unset,
    One(usize),
    All,
    NoChoice,
}

struct Detail {
    server: String,
    root: Item,
    files: Vec<Item>,
    versions: Vec<VersionSummary>,
    choice: VersionChoice,
    seasons: Vec<(i32, bool)>,
}

struct SeasonSelect {
    seasons: Vec<(i32, bool)>,
    state: ListState,
}

enum Purpose {
    OutDir,
    LimitRate,
    Pattern,
}

struct InputModal {
    title: String,
    input: TextInput,
    purpose: Purpose,
}

struct LoginForm {
    server: TextInput,
    name: TextInput,
    user: TextInput,
    password: TextInput,
    api_key: TextInput,
    insecure: bool,
    focus: usize,
}

enum Modal {
    None,
    Detail,
    Seasons(SeasonSelect),
    Input(InputModal),
    Login(LoginForm),
    Url(String),
    Confirm(String),
    Message(String, bool),
    Help,
    Loading(String),
}

enum Msg {
    Searched {
        seq: u64,
        hits: Vec<SearchHit>,
        errors: Vec<String>,
    },
    Loaded {
        seq: u64,
        server: String,
        parent: Option<String>,
        title: String,
        items: Vec<Item>,
        error: Option<String>,
    },
    Prepared(Box<PreparedMsg>),
    LoggedIn {
        seq: u64,
        /// (别名, 客户端, 覆盖同名服务器的警告)
        result: Result<(String, Client, Option<String>), String>,
    },
}

struct PreparedMsg {
    seq: u64,
    server: String,
    root: Item,
    files: Vec<Item>,
    error: Option<String>,
}

struct App {
    cfg: Config,
    clients: BTreeMap<String, Client>,
    tab: Tab,
    quit: bool,
    busy: Option<String>,
    spin: usize,
    status: Option<(String, Kind)>,
    msg_tx: mpsc::UnboundedSender<Msg>,
    search: SearchState,
    browse: BrowseState,
    servers: ListState,
    settings: Settings,
    settings_state: ListState,
    detail: Option<Detail>,
    modal: Modal,
    manager: Manager,
    downloads: ListState,
    download_focus: Option<String>,
    seq: u64,
    prepare_seq: Option<u64>,
    login_seq: Option<u64>,
}

impl App {
    fn new(
        cfg: Config,
        clients: BTreeMap<String, Client>,
        msg_tx: mpsc::UnboundedSender<Msg>,
    ) -> Self {
        // 设置持久化在 config.json 里；读取时做一次范围校正，
        // 避免手工编辑配置后把并发数设成 0/超大值。
        let mut settings = cfg.settings.clone();
        settings.connections = settings.connections.clamp(1, 32);
        settings.jobs = settings.jobs.clamp(1, 8);
        let mut servers = ListState::default();
        if !cfg.servers.is_empty() {
            servers.select(Some(0));
        }
        Self {
            cfg,
            clients,
            tab: Tab::Search,
            quit: false,
            busy: None,
            spin: 0,
            status: None,
            msg_tx,
            search: SearchState::default(),
            browse: BrowseState::default(),
            servers,
            settings,
            settings_state: ListState::default().with_selected(Some(0)),
            detail: None,
            modal: Modal::None,
            manager: Manager::new(),
            downloads: ListState::default().with_selected(Some(0)),
            download_focus: None,
            seq: 0,
            prepare_seq: None,
            login_seq: None,
        }
    }

    /// 把当前设置写回 config.json（保留服务器列表）。
    fn save_settings(&mut self) {
        let settings = self.settings.clone();
        let to_write = settings.clone();
        match config::edit(move |cfg| {
            cfg.settings = to_write;
            Ok(())
        }) {
            Ok(()) => self.cfg.settings = settings,
            Err(e) => self.set_error(format!("保存设置失败：{e:#}")),
        }
    }

    fn tick(&mut self) {
        self.spin = self.spin.wrapping_add(1);
    }

    fn set_info(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), Kind::Info));
    }

    fn set_ok(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), Kind::Ok));
    }

    fn set_warn(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), Kind::Warn));
    }

    fn set_error(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), Kind::Err));
    }

    fn is_typing(&self) -> bool {
        self.tab == Tab::Search && self.search.focus == SearchFocus::Query
    }

    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Searched { seq, hits, errors } => {
                if self.search.pending != Some(seq) {
                    return;
                }
                self.search.pending = None;
                self.busy = None;
                self.search.hits = hits;
                if self.search.hits.is_empty() {
                    self.search.state.select(None);
                    self.search.focus = SearchFocus::Query;
                } else {
                    self.search.state.select(Some(0));
                    self.search.focus = SearchFocus::Results;
                }
                if !errors.is_empty() {
                    self.set_warn(format!("部分服务器搜索失败：{}", errors.join("；")));
                } else if self.search.hits.is_empty() {
                    self.set_warn("没有匹配结果");
                } else {
                    self.set_ok(format!("找到 {} 条结果", self.search.hits.len()));
                }
            }
            Msg::Loaded {
                seq,
                server,
                parent,
                title,
                items,
                error,
            } => {
                if self.browse.pending != Some(seq) {
                    return;
                }
                self.browse.pending = None;
                self.busy = None;
                if let Some(err) = error {
                    self.modal = Modal::Message(format!("加载失败：{err}"), true);
                    return;
                }
                let count = items.len();
                self.browse.levels.push(BrowseLevel {
                    server,
                    parent,
                    title,
                    items,
                });
                self.browse.state.select(if count == 0 { None } else { Some(0) });
                if count == 0 {
                    self.set_warn("这个目录是空的");
                } else {
                    self.set_ok(format!("共 {count} 项"));
                }
            }
            Msg::Prepared(msg) => {
                let PreparedMsg {
                    seq,
                    server,
                    root,
                    files,
                    error,
                } = *msg;
                if self.prepare_seq != Some(seq) {
                    return;
                }
                self.prepare_seq = None;
                self.busy = None;
                if let Some(err) = error {
                    self.modal = Modal::Message(format!("解析条目失败：{err}"), true);
                    return;
                }
                self.open_detail(server, root, files);
            }
            Msg::LoggedIn { seq, result } => {
                if self.login_seq != Some(seq) {
                    return;
                }
                self.login_seq = None;
                self.busy = None;
                match result {
                    Ok((name, client, warning)) => {
                        self.cfg = config::load_optional().ok().flatten().unwrap_or_default();
                        self.clients.insert(name.clone(), client);
                        self.search.server = Some(name.clone());
                        self.tab = Tab::Servers;
                        match warning {
                            Some(w) => self.set_warn(w),
                            None => self.set_ok(format!("已登录 {name}，并设为搜索范围")),
                        }
                    }
                    Err(err) => {
                        self.modal = Modal::Message(format!("登录失败：{err}"), true);
                    }
                }
            }
        }
    }

    // ───────────────────── 按键分发 ─────────────────────

    fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return Ok(());
        }
        if !matches!(self.modal, Modal::None) {
            return self.on_modal_key(key);
        }
        if self.on_global_key(key) {
            return Ok(());
        }
        match self.tab {
            Tab::Search => self.on_search_key(key),
            Tab::Browse => self.on_browse_key(key),
            Tab::Downloads => self.on_downloads_key(key),
            Tab::Servers => self.on_servers_key(key),
            Tab::Settings => self.on_settings_key(key),
        }
    }

    fn on_global_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('?') if !self.is_typing() => {
                self.modal = Modal::Help;
                true
            }
            KeyCode::Char('q') if !self.is_typing() => {
                self.quit = true;
                true
            }
            KeyCode::Tab => {
                self.tab = self.tab.next();
                true
            }
            KeyCode::BackTab => {
                self.tab = self.tab.prev();
                true
            }
            KeyCode::Char(c @ '1'..='5') if !self.is_typing() => {
                match Tab::from_digit(c) {
                    Some(tab) => {
                        self.tab = tab;
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        }
    }

    fn on_search_key(&mut self, key: KeyEvent) -> Result<()> {
        match self.search.focus {
            SearchFocus::Query => match key.code {
                KeyCode::Enter => {
                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                        return Ok(());
                    }
                    self.start_search();
                }
                KeyCode::Down if !self.search.hits.is_empty() => {
                    self.search.focus = SearchFocus::Results;
                    if self.search.state.selected().is_none() {
                        self.search.state.select(Some(0));
                    }
                }
                KeyCode::Esc => {
                    self.search.query = TextInput::default();
                }
                _ => {
                    self.search.query.handle(&key);
                }
            },
            SearchFocus::Results => match key.code {
                KeyCode::Up => {
                    let cur = self.search.state.selected().unwrap_or(0);
                    if cur == 0 {
                        self.search.focus = SearchFocus::Query;
                    } else {
                        self.search.state.select(Some(cur - 1));
                    }
                }
                KeyCode::Down => {
                    let cur = self.search.state.selected().unwrap_or(0);
                    if cur + 1 < self.search.hits.len() {
                        self.search.state.select(Some(cur + 1));
                    }
                }
                KeyCode::Enter | KeyCode::Char('d') => {
                    if let Some(idx) = self.search.state.selected() {
                        if let Some(hit) = self.search.hits.get(idx).cloned() {
                            self.start_prepare(hit.server, hit.item);
                        }
                    } else {
                        self.search.focus = SearchFocus::Query;
                    }
                }
                KeyCode::Char('/') | KeyCode::Esc => {
                    self.search.focus = SearchFocus::Query;
                }
                _ => {}
            },
        }
        Ok(())
    }

    fn on_browse_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.browse.levels.is_empty() {
            if self.browse.server_state.selected().is_none() && !self.cfg.servers.is_empty() {
                self.browse.server_state.select(Some(0));
            }
            match key.code {
                KeyCode::Up => move_sel(&mut self.browse.server_state, self.cfg.servers.len(), -1),
                KeyCode::Down => move_sel(&mut self.browse.server_state, self.cfg.servers.len(), 1),
                KeyCode::Enter => {
                    if let Some(name) =
                        selected_server(&self.cfg, self.browse.server_state.selected())
                    {
                        self.start_load(name, None);
                    }
                }
                KeyCode::Esc => {}
                _ => {}
            }
            return Ok(());
        }

        match key.code {
            KeyCode::Up => {
                let len = self.browse.levels.last().map(|l| l.items.len()).unwrap_or(0);
                move_sel(&mut self.browse.state, len, -1);
            }
            KeyCode::Down => {
                let len = self.browse.levels.last().map(|l| l.items.len()).unwrap_or(0);
                move_sel(&mut self.browse.state, len, 1);
            }
            KeyCode::Enter => {
                if let Some((server, item)) = self.current_browse_item() {
                    if item.is_container() {
                        self.start_load(server, Some((item.id.clone(), item.title())));
                    } else {
                        self.start_prepare(server, item);
                    }
                }
            }
            KeyCode::Char('d') => {
                if let Some((server, item)) = self.current_browse_item() {
                    self.start_prepare(server, item);
                }
            }
            KeyCode::Backspace | KeyCode::Esc => {
                self.browse.levels.pop();
                self.browse.state.select(Some(0));
            }
            KeyCode::Char('r') => {
                if let Some(level) = self.browse.levels.pop() {
                    let parent = level.parent.map(|id| (id, level.title.clone()));
                    self.start_load(level.server, parent);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn on_servers_key(&mut self, key: KeyEvent) -> Result<()> {
        let len = self.cfg.servers.len();
        match key.code {
            KeyCode::Up => move_sel(&mut self.servers, len, -1),
            KeyCode::Down => move_sel(&mut self.servers, len, 1),
            KeyCode::Char('a') => {
                self.modal = Modal::Login(LoginForm {
                    server: TextInput::default(),
                    name: TextInput::default(),
                    user: TextInput::default(),
                    password: TextInput::default(),
                    api_key: TextInput::default(),
                    insecure: false,
                    focus: 0,
                });
            }
            KeyCode::Char('d') => {
                if let Some(name) = selected_server(&self.cfg, self.servers.selected()) {
                    self.modal = Modal::Confirm(name);
                }
            }
            KeyCode::Char('l') | KeyCode::Enter => {
                if let Some(name) = selected_server(&self.cfg, self.servers.selected()) {
                    if key.code == KeyCode::Enter {
                        // Enter 切换搜索范围
                        if self.search.server.as_deref() == Some(name.as_str()) {
                            self.search.server = None;
                            self.set_info("搜索范围：全部服务器");
                        } else {
                            self.search.server = Some(name.clone());
                            self.set_info(format!("搜索范围：{name}"));
                        }
                    } else {
                        self.tab = Tab::Browse;
                        self.browse.levels.clear();
                        self.start_load(name, None);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    const SETTING_ROWS: usize = 8;

    fn on_settings_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Up => move_sel(&mut self.settings_state, Self::SETTING_ROWS, -1),
            KeyCode::Down => move_sel(&mut self.settings_state, Self::SETTING_ROWS, 1),
            KeyCode::Left => self.adjust_setting(-1),
            KeyCode::Right => self.adjust_setting(1),
            KeyCode::Char(' ') => self.toggle_setting(),
            KeyCode::Enter => self.edit_setting(),
            _ => {}
        }
        Ok(())
    }

    fn on_downloads_key(&mut self, key: KeyEvent) -> Result<()> {
        let count = self.manager.state().servers.len();
        match key.code {
            KeyCode::Up => move_sel(&mut self.downloads, count, -1),
            KeyCode::Down => move_sel(&mut self.downloads, count, 1),
            KeyCode::Char('x') => {
                let name = {
                    let st = self.manager.state();
                    let idx = self.downloads.selected().unwrap_or(0);
                    st.servers.keys().nth(idx).cloned()
                };
                if let Some(name) = name {
                    self.manager.cancel_server(&name);
                    self.set_warn(format!("已取消 {name} 的下载队列"));
                }
            }
            KeyCode::Char('X') => {
                self.manager.cancel_all();
                self.set_warn("已取消所有服务器的下载");
            }
            KeyCode::Char('c') => {
                self.manager.clear_finished();
                self.set_info("已清空下载历史");
            }
            _ => {}
        }
        Ok(())
    }

    fn adjust_setting(&mut self, delta: isize) {
        let changed = match self.settings_state.selected().unwrap_or(0) {
            1 => {
                self.settings.connections =
                    (self.settings.connections as isize + delta).clamp(1, 32) as usize;
                true
            }
            2 => {
                self.settings.jobs =
                    (self.settings.jobs as isize + delta).clamp(1, 8) as usize;
                true
            }
            4 => {
                self.settings.subs = !self.settings.subs;
                true
            }
            5 => {
                self.settings.overwrite = !self.settings.overwrite;
                true
            }
            6 => {
                self.settings.resume = !self.settings.resume;
                true
            }
            _ => false,
        };
        if changed {
            self.save_settings();
        }
    }

    fn toggle_setting(&mut self) {
        let changed = match self.settings_state.selected().unwrap_or(0) {
            4 => {
                self.settings.subs = !self.settings.subs;
                true
            }
            5 => {
                self.settings.overwrite = !self.settings.overwrite;
                true
            }
            6 => {
                self.settings.resume = !self.settings.resume;
                true
            }
            _ => false,
        };
        if changed {
            self.save_settings();
        }
    }

    fn edit_setting(&mut self) {
        let (title, value, purpose) = match self.settings_state.selected().unwrap_or(0) {
            0 => ("输出目录", self.settings.out.clone(), Purpose::OutDir),
            3 => ("限速（如 5M，留空不限速）", self.settings.limit.clone(), Purpose::LimitRate),
            7 => ("命名模板（留空使用默认）", self.settings.pattern.clone(), Purpose::Pattern),
            _ => return,
        };
        self.modal = Modal::Input(InputModal {
            title: title.into(),
            input: TextInput::new(&value),
            purpose,
        });
    }

    // ───────────────────── 后台任务 ─────────────────────

    fn start_search(&mut self) {
        let term = self.search.query.text.trim().to_string();
        if term.is_empty() {
            self.set_warn("请先输入搜索关键字");
            return;
        }
        if term.starts_with("http://") || term.starts_with("https://") {
            let item = crate::direct_item(&term);
            self.open_detail(DIRECT_SERVER.to_string(), item.clone(), vec![item]);
            return;
        }
        let targets: Vec<(String, Client)> = match &self.search.server {
            Some(name) => self
                .clients
                .get(name)
                .map(|c| vec![(name.clone(), c.clone())])
                .unwrap_or_default(),
            None => self
                .clients
                .iter()
                .map(|(n, c)| (n.clone(), c.clone()))
                .collect(),
        };
        if targets.is_empty() {
            self.set_error("没有可用的服务器，请先在「服务器」页登录");
            return;
        }

        let seq = self.next_seq();
        self.search.pending = Some(seq);
        self.busy = Some(format!("搜索 “{term}”…"));
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let mut hits = Vec::new();
            let mut errors = Vec::new();
            for (name, client) in targets {
                match client.search(&term, SEARCH_LIMIT).await {
                    Ok(items) => hits.extend(items.into_iter().map(|item| SearchHit {
                        server: name.clone(),
                        item,
                    })),
                    Err(e) => errors.push(format!("{name}: {e:#}")),
                }
            }
            hits.sort_by(|a, b| {
                a.item
                    .title()
                    .cmp(&b.item.title())
                    .then_with(|| a.server.cmp(&b.server))
            });
            let _ = tx.send(Msg::Searched { seq, hits, errors });
        });
    }

    fn start_load(&mut self, server: String, parent: Option<(String, String)>) {
        let Some(client) = self.clients.get(&server).cloned() else {
            self.set_error(format!("服务器 {server} 未连接"));
            return;
        };
        let seq = self.next_seq();
        self.browse.pending = Some(seq);
        let title = parent
            .as_ref()
            .map(|(_, t)| t.clone())
            .unwrap_or_else(|| "媒体库".to_string());
        self.busy = Some(format!("加载 {title}…"));
        let tx = self.msg_tx.clone();
        let parent_id = parent.map(|(id, _)| id);
        let parent_for_msg = parent_id.clone();
        tokio::spawn(async move {
            let result = match &parent_id {
                None => client.views().await,
                Some(id) => client.children(id, false, None, None).await,
            };
            let (items, error) = match result {
                Ok(items) => (items, None),
                Err(e) => (Vec::new(), Some(format!("{e:#}"))),
            };
            let _ = tx.send(Msg::Loaded {
                seq,
                server,
                parent: parent_for_msg,
                title,
                items,
                error,
            });
        });
    }

    fn start_prepare(&mut self, server: String, item: Item) {
        let Some(client) = self.clients.get(&server).cloned() else {
            self.set_error(format!("服务器 {server} 未连接"));
            return;
        };
        let seq = self.next_seq();
        self.prepare_seq = Some(seq);
        self.busy = Some(format!("解析 {}…", item.title()));
        self.modal = Modal::Loading(format!("正在解析 “{}”…", item.title()));
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let result = resolve_item(&client, item.clone(), &[]).await;
            let msg = match result {
                Ok(files) => PreparedMsg {
                    seq,
                    server,
                    root: item,
                    files,
                    error: None,
                },
                Err(e) => PreparedMsg {
                    seq,
                    server,
                    root: item,
                    files: Vec::new(),
                    error: Some(format!("{e:#}")),
                },
            };
            let _ = tx.send(Msg::Prepared(Box::new(msg)));
        });
    }

    fn current_browse_item(&self) -> Option<(String, Item)> {
        let level = self.browse.levels.last()?;
        let idx = self.browse.state.selected()?;
        let item = level.items.get(idx)?.clone();
        Some((level.server.clone(), item))
    }

    fn open_detail(&mut self, server: String, root: Item, files: Vec<Item>) {
        // 只有“同一集有多个媒体源 / 同一集出现多条”才算多版本。
        // 单源条目文件名里的 1080p、发布组等只是片源信息；据此过滤会把
        // 未标注的集静默丢掉。
        let versions = if has_multiple_versions(&files) {
            version_summaries(&files)
        } else {
            Vec::new()
        };
        let covered = versions
            .first()
            .map(|v| v.count >= files.len())
            .unwrap_or(false);
        let choice = if versions.is_empty() {
            VersionChoice::NoChoice
        } else if versions.len() == 1 && covered {
            VersionChoice::One(0)
        } else {
            VersionChoice::Unset
        };
        let mut seasons: Vec<i32> = files
            .iter()
            .filter(|i| i.item_type.as_deref() == Some("Episode"))
            .map(|i| i.season_no())
            .collect();
        seasons.sort_unstable();
        seasons.dedup();
        let seasons = seasons.into_iter().map(|s| (s, true)).collect();
        self.detail = Some(Detail {
            server,
            root,
            files,
            versions,
            choice,
            seasons,
        });
        self.modal = Modal::Detail;
    }

    fn start_delete(&mut self, name: &str) {
        match config::edit(|cfg| {
            cfg.servers.remove(name);
            Ok(cfg.clone())
        }) {
            Ok(cfg) => {
                self.cfg = cfg;
                self.clients.remove(name);
                if self.search.server.as_deref() == Some(name) {
                    self.search.server = None;
                }
                self.set_ok(format!("已删除服务器 {name}"));
            }
            Err(e) => self.set_error(format!("保存配置失败：{e:#}")),
        }
    }

    fn submit_login(&mut self, form: &LoginForm) {
        let server = form.server.text.trim().to_string();
        let name = form.name.text.trim().to_string();
        let user = form.user.text.trim().to_string();
        let password = form.password.text.clone();
        let api_key = form.api_key.text.trim().to_string();
        let insecure = form.insecure;

        if server.is_empty() {
            self.modal = Modal::Message("请填写服务器地址".into(), true);
            return;
        }
        if api_key.is_empty() && (user.is_empty() || password.is_empty()) {
            self.modal = Modal::Message(
                "请填写 API Key，或同时填写用户名和密码".into(),
                true,
            );
            return;
        }

        let seq = self.next_seq();
        self.login_seq = Some(seq);
        self.busy = Some(format!("登录 {server}…"));
        self.modal = Modal::None;
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let result: Result<(String, Client, Option<String>), String> = async {
                let mut client = Client::new(&server, insecure).map_err(|e| format!("{e:#}"))?;
                let username = if !api_key.is_empty() {
                    client.token = api_key.clone();
                    client.resolve_user().await.map_err(|e| format!("{e:#}"))?;
                    None
                } else {
                    client
                        .login(&user, &password)
                        .await
                        .map_err(|e| format!("{e:#}"))?;
                    client.resolve_user().await.map_err(|e| format!("{e:#}"))?;
                    Some(user.clone())
                };
                let name = if name.is_empty() {
                    config::name_from_server(&client.server)
                } else {
                    name.clone()
                };
                let profile = Profile {
                    server: client.server.clone(),
                    token: client.token.clone(),
                    user_id: client.user_id.clone(),
                    username,
                    insecure,
                };
                let replaced = config::edit(|cfg| Ok(cfg.servers.insert(name.clone(), profile)))
                    .map_err(|e| format!("{e:#}"))?;
                // 与 CLI 一致：覆盖了指向另一个地址的同名服务器时给出警告。
                let warning = replaced
                    .filter(|old| old.server != client.server)
                    .map(|old| {
                        format!(
                            "⚠️  已覆盖同名服务器 {name}（原地址 {}）",
                            old.server
                        )
                    });
                Ok((name, client, warning))
            }
            .await;
            let _ = tx.send(Msg::LoggedIn { seq, result });
        });
    }

    // ───────────────────── 下载 ─────────────────────

    fn build_opts(&self) -> Result<DownloadOpts> {
        let limit = self.settings.limit.trim();
        let limit_rate = if limit.is_empty() {
            None
        } else {
            Some(parse_rate(limit).with_context(|| format!("无法解析限速值：{limit}"))?)
        };
        let out = self.settings.out.trim();
        let out = if out.is_empty() { "./downloads" } else { out };
        Ok(DownloadOpts {
            out: crate::util::expand_tilde(Path::new(out))?,
            pattern: Some(self.settings.pattern.clone()).filter(|p| !p.trim().is_empty()),
            connections: self.settings.connections.clamp(1, 32),
            jobs: self.settings.jobs.clamp(1, 8),
            limit_rate,
            resume: self.settings.resume,
            overwrite: self.settings.overwrite,
            subs: self.settings.subs,
            dry_run: false,
        })
    }

    fn build_download(&self, d: &Detail) -> Result<(Client, Vec<Item>, DownloadOpts, usize)> {
        let client = if d.server == DIRECT_SERVER {
            Client::new("http://127.0.0.1", false).context("构建直链下载客户端失败")?
        } else {
            self.clients
                .get(&d.server)
                .cloned()
                .with_context(|| format!("服务器 {} 未连接", d.server))?
        };
        let mut items = d.files.clone();
        let all_seasons = d.seasons.iter().all(|(_, on)| *on);
        if !d.seasons.is_empty() && !all_seasons {
            items.retain(|it| {
                if it.item_type.as_deref() == Some("Episode") {
                    let season = it.season_no();
                    d.seasons.iter().any(|(s, on)| *on && *s == season)
                } else {
                    true
                }
            });
        }
        if items.is_empty() {
            bail!("所选季下没有可下载的条目");
        }
        let mut skipped = 0usize;
        items = match &d.choice {
            VersionChoice::Unset => bail!(
                "请先选择版本：按 1-{} 选择，0 或 a 下载全部版本",
                d.versions.len()
            ),
            VersionChoice::All => expand_versions(items),
            VersionChoice::One(i) => {
                let label = d
                    .versions
                    .get(*i)
                    .with_context(|| format!("版本序号超出范围：{}", i + 1))?
                    .label
                    .clone();
                let (kept, skipped_n) = filter_to_version(items, &label);
                skipped = skipped_n;
                kept
            }
            VersionChoice::NoChoice => items,
        };
        if items.is_empty() {
            bail!("所选版本下没有可下载的条目");
        }
        Ok((client, items, self.build_opts()?, skipped))
    }

    /// 加入后台下载队列：界面保持可用，每个服务器独立排队。
    fn start_download(&mut self, d: Detail) {
        match self.build_download(&d) {
            Ok((client, items, opts, skipped)) => {
                let count = items.len();
                let server = d.server.clone();
                self.manager.enqueue(server.clone(), client, items, opts);
                self.detail = None;
                self.modal = Modal::None;
                self.tab = Tab::Downloads;
                self.download_focus = Some(server.clone());
                let note = if skipped > 0 {
                    format!("，跳过 {skipped} 个不含所选版本的条目")
                } else {
                    String::new()
                };
                self.set_ok(format!(
                    "已加入 {server} 的下载队列（{count} 个文件{note}）"
                ));
            }
            Err(e) => {
                self.detail = Some(d);
                self.modal = Modal::Detail;
                self.set_error(format!("{e:#}"));
            }
        }
    }

    fn detail_total_size(&self, d: &Detail) -> Option<u64> {
        match d.choice {
            VersionChoice::One(i) => d.versions.get(i).and_then(|v| v.size),
            VersionChoice::All => {
                let mut total = 0u64;
                let mut any = false;
                for v in &d.versions {
                    if let Some(s) = v.size {
                        total += s;
                        any = true;
                    }
                }
                any.then_some(total)
            }
            VersionChoice::NoChoice | VersionChoice::Unset => {
                let mut total = 0u64;
                let mut any = false;
                for it in &d.files {
                    if let Some(s) = it.size() {
                        total += s;
                        any = true;
                    }
                }
                any.then_some(total)
            }
        }
    }

    // ───────────────────── 弹窗按键 ─────────────────────

    fn on_modal_key(&mut self, key: KeyEvent) -> Result<()> {
        let modal = std::mem::replace(&mut self.modal, Modal::None);
        match modal {
            Modal::None => {}
            Modal::Loading(msg) => {
                self.modal = Modal::Loading(msg);
            }
            Modal::Message(..) | Modal::Help | Modal::Url(..) => {
                // 任意键关闭；若详情仍在，回到详情
                self.modal = if self.detail.is_some() {
                    Modal::Detail
                } else {
                    Modal::None
                };
            }
            Modal::Confirm(name) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.start_delete(&name),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {}
                _ => self.modal = Modal::Confirm(name),
            },
            Modal::Input(mut im) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => match self.apply_input(&im) {
                    Ok(()) => {}
                    Err(e) => self.modal = Modal::Message(format!("{e:#}"), true),
                },
                _ => {
                    im.input.handle(&key);
                    self.modal = Modal::Input(im);
                }
            },
            Modal::Seasons(mut ss) => match key.code {
                KeyCode::Esc => {
                    self.modal = Modal::Detail;
                }
                KeyCode::Enter => {
                    if let Some(d) = self.detail.as_mut() {
                        d.seasons = ss.seasons;
                    }
                    self.modal = Modal::Detail;
                }
                KeyCode::Up => {
                    move_sel(&mut ss.state, ss.seasons.len(), -1);
                    self.modal = Modal::Seasons(ss);
                }
                KeyCode::Down => {
                    move_sel(&mut ss.state, ss.seasons.len(), 1);
                    self.modal = Modal::Seasons(ss);
                }
                KeyCode::Char(' ') => {
                    if let Some(idx) = ss.state.selected()
                        && let Some((_, on)) = ss.seasons.get_mut(idx)
                    {
                        *on = !*on;
                    }
                    self.modal = Modal::Seasons(ss);
                }
                KeyCode::Char('a') => {
                    let all = ss.seasons.iter().all(|(_, on)| *on);
                    for (_, on) in &mut ss.seasons {
                        *on = !all;
                    }
                    self.modal = Modal::Seasons(ss);
                }
                _ => self.modal = Modal::Seasons(ss),
            },
            Modal::Login(mut form) => {
                let submit = match key.code {
                    KeyCode::Esc => {
                        return Ok(());
                    }
                    KeyCode::Tab | KeyCode::Down => {
                        form.focus = (form.focus + 1) % 6;
                        false
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        form.focus = (form.focus + 5) % 6;
                        false
                    }
                    KeyCode::Enter => {
                        if form.focus == 5 {
                            true
                        } else {
                            form.focus = (form.focus + 1) % 6;
                            false
                        }
                    }
                    KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => true,
                    KeyCode::Char(' ') if form.focus == 5 => {
                        form.insecure = !form.insecure;
                        false
                    }
                    _ => {
                        match form.focus {
                            0 => {
                                form.server.handle(&key);
                            }
                            1 => {
                                form.name.handle(&key);
                            }
                            2 => {
                                form.user.handle(&key);
                            }
                            3 => {
                                form.password.handle(&key);
                            }
                            4 => {
                                form.api_key.handle(&key);
                            }
                            _ => {}
                        }
                        false
                    }
                };
                if submit {
                    self.submit_login(&form);
                } else {
                    self.modal = Modal::Login(form);
                }
            }
            Modal::Detail => {
                if self.detail.is_none() {
                    return Ok(());
                }
                if key.code == KeyCode::Enter {
                    if let Some(d) = self.detail.take() {
                        self.start_download(d);
                    }
                    return Ok(());
                }
                let mut open_seasons = false;
                let mut url: Option<String> = None;
                let mut close = false;
                if let Some(d) = self.detail.as_mut() {
                    match key.code {
                        KeyCode::Esc => close = true,
                        KeyCode::Char('f') => open_seasons = !d.seasons.is_empty(),
                        KeyCode::Char('u') => {
                            if let Some(item) = d.files.first() {
                                if let Some(direct) = item.direct_url.clone() {
                                    url = Some(format!("（直链）\n\n{direct}"));
                                } else if let Some(client) = self.clients.get(&d.server)
                                    && let Some(u) = client.media_urls(item).first()
                                {
                                    let note = if d.files.len() > 1 {
                                        format!(
                                            "（{}，共 {} 集；这里显示第一集）",
                                            item.title(),
                                            d.files.len()
                                        )
                                    } else {
                                        format!("（{}）", item.title())
                                    };
                                    url = Some(format!("{note}\n\n{u}"));
                                }
                            }
                        }
                        KeyCode::Char(c @ '1'..='9') => {
                            if let Some(n) = c.to_digit(10) {
                                let idx = n as usize - 1;
                                if idx < d.versions.len() {
                                    d.choice = VersionChoice::One(idx);
                                }
                            }
                        }
                        KeyCode::Char('0') | KeyCode::Char('a')
                            if !d.versions.is_empty() =>
                        {
                            d.choice = VersionChoice::All;
                        }
                        _ => {}
                    }
                }
                if close {
                    self.detail = None;
                    self.modal = Modal::None;
                } else if let Some(u) = url {
                    self.modal = Modal::Url(u);
                } else if open_seasons {
                    if let Some(d) = self.detail.as_ref() {
                        self.modal = Modal::Seasons(SeasonSelect {
                            seasons: d.seasons.clone(),
                            state: ListState::default().with_selected(Some(0)),
                        });
                    } else {
                        self.modal = Modal::Detail;
                    }
                } else {
                    self.modal = Modal::Detail;
                }
            }
        }
        Ok(())
    }

    fn apply_input(&mut self, im: &InputModal) -> Result<()> {
        match im.purpose {
            Purpose::OutDir => {
                self.settings.out = im.input.text.clone();
            }
            Purpose::LimitRate => {
                let value = im.input.text.trim();
                if !value.is_empty() && parse_rate(value).is_none() {
                    bail!("无法解析限速值 “{value}”，示例：5M、500K");
                }
                self.settings.limit = value.to_string();
            }
            Purpose::Pattern => {
                self.settings.pattern = im.input.text.clone();
            }
        }
        // 输出目录/限速/模板修改后立即落盘，重启 TUI 仍然生效。
        self.save_settings();
        Ok(())
    }

    // ───────────────────── 渲染 ─────────────────────

    fn draw(&mut self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(2),
            ])
            .split(f.area());

        self.draw_header(f, chunks[0]);
        match self.tab {
            Tab::Search => self.draw_search(f, chunks[1]),
            Tab::Browse => self.draw_browse(f, chunks[1]),
            Tab::Downloads => self.draw_downloads(f, chunks[1]),
            Tab::Servers => self.draw_servers(f, chunks[1]),
            Tab::Settings => self.draw_settings(f, chunks[1]),
        }
        self.draw_footer(f, chunks[2]);
        self.draw_modal(f);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let mut title = vec![
            Span::styled(" embydown ", accent()),
            Span::styled(
                format!(" {} 个服务器 ", self.cfg.servers.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ];
        if let Some(busy) = &self.busy {
            title.push(Span::styled(
                format!("{} {} ", spinner(self.spin), busy),
                Style::default().fg(Color::Yellow),
            ));
        }
        let block = Block::default().borders(Borders::ALL).title(Line::from(title));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let tabs = Tabs::new(vec![" 搜索 ", " 浏览 ", " 下载 ", " 服务器 ", " 设置 "])
            .select(self.tab.index())
            .highlight_style(accent());
        f.render_widget(tabs, inner);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let status = match &self.status {
            Some((text, kind)) => {
                let color = match kind {
                    Kind::Info => Color::Gray,
                    Kind::Ok => Color::Green,
                    Kind::Warn => Color::Yellow,
                    Kind::Err => Color::Red,
                };
                Span::styled(text.clone(), Style::default().fg(color))
            }
            None => Span::styled("就绪", Style::default().fg(Color::DarkGray)),
        };
        let lines = vec![
            Line::from(Span::styled(self.hints(), Style::default().fg(Color::DarkGray))),
            Line::from(status),
        ];
        f.render_widget(Paragraph::new(lines), area);
    }

    fn hints(&self) -> &'static str {
        match (self.tab, self.search.focus) {
            (Tab::Search, SearchFocus::Query) => {
                "Enter 搜索 · ↓ 结果 · Tab 切页 · Esc 清空 · ? 帮助"
            }
            (Tab::Search, SearchFocus::Results) => {
                "按 / 输入关键字 · ↑↓ 选择 · Enter 打开 · Tab 切页 · ? 帮助 · q 退出"
            }
            (Tab::Browse, _) if self.browse.levels.is_empty() => {
                "↑↓ 选择服务器 · Enter 进入 · 1-4 切页 · ? 帮助 · q 退出"
            }
            (Tab::Browse, _) => {
                "↑↓ 选择 · Enter 进入/下载 · d 直接下载 · Backspace 返回 · Tab 切页 · q 退出"
            }
            (Tab::Downloads, _) => {
                "↑↓ 选择服务器 · x 取消该服务器 · X 取消全部 · c 清空历史 · Tab 切页 · q 退出"
            }
            (Tab::Servers, _) => {
                "↑↓ 选择 · Enter 设为搜索范围 · l 浏览 · a 登录 · d 删除 · Tab 切页 · q 退出"
            }
            (Tab::Settings, _) => {
                "↑↓ 选择 · ←→ 调整 · Enter 编辑 · 空格 切换 · Tab 切页 · q 退出"
            }
        }
    }

    fn draw_search(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(3)])
            .split(area);

        let scope = self.search.server.as_deref().unwrap_or("全部服务器");
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" 搜索 · 范围：{scope} "));
        let inner = block.inner(chunks[0]);
        f.render_widget(block, chunks[0]);
        let focused = self.search.focus == SearchFocus::Query;
        let line = if self.search.query.text.is_empty() && !focused {
            Line::from(Span::styled(
                "按 / 输入关键字或 http(s) 直链",
                Style::default().fg(Color::DarkGray),
            ))
        } else {
            self.search.query.render_line("关键字: ", focused, false)
        };
        f.render_widget(Paragraph::new(line), inner);

        let header = Row::new(vec![
            Cell::from("服务器"),
            Cell::from("类型"),
            Cell::from("年份"),
            Cell::from("季"),
            Cell::from("大小"),
            Cell::from("名称"),
        ])
        .style(accent());

        let rows: Vec<Row> = self
            .search
            .hits
            .iter()
            .map(|h| {
                Row::new(vec![
                    Cell::from(h.server.clone()),
                    Cell::from(h.item.display_type().to_string()),
                    Cell::from(year_label(&h.item)),
                    Cell::from(season_label(&h.item)),
                    Cell::from(h.item.size().map(format_bytes).unwrap_or_else(|| "-".into())),
                    Cell::from(h.item.title()),
                ])
            })
            .collect();

        let title = if self.search.hits.is_empty() {
            " 结果 ".to_string()
        } else {
            format!(" 结果（{}） ", self.search.hits.len())
        };
        let table = Table::new(
            rows,
            [
                Constraint::Length(20),
                Constraint::Length(8),
                Constraint::Length(5),
                Constraint::Length(7),
                Constraint::Length(10),
                Constraint::Min(12),
            ],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title))
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
        f.render_stateful_widget(table, chunks[1], &mut self.search.state);
    }

    fn draw_browse(&mut self, f: &mut Frame, area: Rect) {
        if self.browse.levels.is_empty() {
            let names: Vec<String> = self.cfg.servers.keys().cloned().collect();
            let items: Vec<ListItem> = names
                .iter()
                .map(|name| {
                    let profile = &self.cfg.servers[name];
                    let user = profile.username.as_deref().unwrap_or("API Key");
                    ListItem::new(Line::from(vec![
                        Span::styled(" 服务器  ", accent()),
                        Span::styled(name.clone(), Style::default().fg(Color::White)),
                        Span::styled(
                            format!("  {} · {user}", profile.server),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]))
                })
                .collect();
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" 浏览 · 选择服务器 "),
                )
                .highlight_style(Style::default().bg(Color::DarkGray))
                .highlight_symbol("› ");
            f.render_stateful_widget(list, area, &mut self.browse.server_state);
            return;
        }

        let level = self.browse.levels.last().unwrap();
        let title = format!(" 浏览 · {} / {} ", level.server, level.title);
        let items: Vec<ListItem> = level
            .items
            .iter()
            .map(|item| {
                let tag = type_tag(item);
                let meta = item_meta(item);
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!(" [{tag}] "),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(item.title(), Style::default().fg(Color::White)),
                    Span::styled(meta, Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
        f.render_stateful_widget(list, area, &mut self.browse.state);
    }

    fn draw_downloads(&mut self, f: &mut Frame, area: Rect) {
        // 队列刚加入时把选中行切到对应服务器
        if let Some(name) = self.download_focus.clone() {
            let idx = {
                let st = self.manager.state();
                st.servers.keys().position(|k| k == &name)
            };
            if let Some(idx) = idx {
                self.downloads.select(Some(idx));
                self.download_focus = None;
            }
        }

        let server_count = self.manager.state().servers.len();
        let mut total_active = 0usize;
        let mut total_queue = 0usize;
        let mut lines: Vec<Line<'static>> = Vec::new();
        {
            let st = self.manager.state();
            let selected = self.downloads.selected().unwrap_or(0);
            for (idx, (name, ss)) in st.servers.iter().enumerate() {
                total_active += ss.active.len();
                total_queue += ss.queue.len();
                let mark = if idx == selected { "▶" } else { " " };
                lines.push(Line::from(vec![
                    Span::styled(format!("{mark} "), accent()),
                    Span::styled(
                        name.clone(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(
                            "  运行 {} · 排队 {} · 完成 {} · 跳过 {} · 失败 {} · 取消 {} · 字幕 {} · {}",
                            ss.active.len(),
                            ss.queue.len(),
                            ss.done,
                            ss.skipped,
                            ss.failed,
                            ss.cancelled,
                            ss.subs,
                            format_bytes(ss.bytes)
                        ),
                        Style::default().fg(Color::Gray),
                    ),
                ]));
                for a in &ss.active {
                    let pos = a.pb.position();
                    let len = a.pb.length().unwrap_or(0);
                    let frac = if len > 0 {
                        (pos as f64 / len as f64).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let speed = pos as f64 / a.started.elapsed().as_secs_f64().max(0.001);
                    let total = if len > 0 {
                        format!("{}/{}", format_bytes(pos), format_bytes(len))
                    } else {
                        format_bytes(pos)
                    };
                    lines.push(Line::from(vec![
                        Span::styled("    ", Style::default()),
                        Span::styled(progress_bar(frac, 18), Style::default().fg(Color::Cyan)),
                        Span::styled(
                            format!(
                                " {:>3.0}%  {total:>21}  {:>10}/s  ",
                                frac * 100.0,
                                format_bytes(speed as u64)
                            ),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::raw(a.title.clone()),
                    ]));
                }
                for q in ss.queue.iter().take(2) {
                    lines.push(Line::from(Span::styled(
                        format!("   排队  {}", q.title),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                if ss.queue.len() > 2 {
                    lines.push(Line::from(Span::styled(
                        format!("   排队  …… 还有 {} 个", ss.queue.len() - 2),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                lines.push(Line::from(""));
            }
        }

        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                " 还没有下载任务。到「搜索」或「浏览」里选中条目，按 Enter 加入队列。",
                Style::default().fg(Color::Yellow),
            )));
        }
        if server_count > 0 {
            if self
                .downloads
                .selected()
                .map(|i| i >= server_count)
                .unwrap_or(true)
            {
                self.downloads.select(Some(server_count - 1));
            }
        } else {
            self.downloads.select(None);
        }

        let mut hist: Vec<Line<'static>> = Vec::new();
        {
            let st = self.manager.state();
            for f in st.finished.iter().rev().take(8) {
                let (mark, color) = match f.status {
                    FinishedStatus::Done => ("✓", Color::Green),
                    FinishedStatus::Skipped => ("⏭", Color::Gray),
                    FinishedStatus::Failed => ("✗", Color::Red),
                    FinishedStatus::Cancelled => ("○", Color::Yellow),
                };
                hist.push(Line::from(vec![
                    Span::styled(format!(" {mark} "), Style::default().fg(color)),
                    Span::styled(format!("{}  ", f.server), Style::default().fg(Color::DarkGray)),
                    Span::raw(f.title.clone()),
                    Span::styled(
                        format!("  {}", format_bytes(f.bytes)),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            if hist.is_empty() {
                hist.push(Line::from(Span::styled(
                    " 暂无完成记录",
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(10)])
            .split(area);
        let title = format!(" 下载队列（运行 {total_active} · 排队 {total_queue}） ");
        let block = Block::default().borders(Borders::ALL).title(title);
        f.render_widget(
            Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
            chunks[0],
        );
        let hblock = Block::default().borders(Borders::ALL).title(" 最近完成 ");
        f.render_widget(Paragraph::new(hist).block(hblock), chunks[1]);
    }

    fn draw_servers(&mut self, f: &mut Frame, area: Rect) {
        let mut items = Vec::new();
        for (name, profile) in self.cfg.servers.iter() {
            let user = profile.username.as_deref().unwrap_or("API Key");
            let mark = if self.search.server.as_deref() == Some(name.as_str()) {
                Span::styled(" ● 搜索范围 ", Style::default().fg(Color::Green))
            } else {
                Span::raw("")
            };
            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!(" {}", pad_display(name, 22)), accent()),
                Span::styled(
                    pad_display(&profile.server, 34),
                    Style::default().fg(Color::White),
                ),
                Span::styled(pad_display(user, 14), Style::default().fg(Color::Gray)),
                mark,
            ])));
        }
        if items.is_empty() {
            items.push(ListItem::new(Line::from(Span::styled(
                "  还没有服务器。按 a 添加（支持账号密码或 API Key）。",
                Style::default().fg(Color::Yellow),
            ))));
        }
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" 服务器 "),
            )
            .highlight_style(Style::default().bg(Color::DarkGray))
            .highlight_symbol("› ");
        f.render_stateful_widget(list, area, &mut self.servers);
    }

    fn draw_settings(&mut self, f: &mut Frame, area: Rect) {
        let rows = [
            ("输出目录", self.settings.out.clone()),
            ("每文件连接数", self.settings.connections.to_string()),
            ("每服务器并发数", self.settings.jobs.to_string()),
            (
                "限速",
                if self.settings.limit.trim().is_empty() {
                    "不限".to_string()
                } else {
                    self.settings.limit.clone()
                },
            ),
            ("下载外挂字幕", on_off(self.settings.subs)),
            ("覆盖已存在文件", on_off(self.settings.overwrite)),
            ("断点续传", on_off(self.settings.resume)),
            (
                "命名模板",
                if self.settings.pattern.trim().is_empty() {
                    "（默认）".to_string()
                } else {
                    self.settings.pattern.clone()
                },
            ),
        ];
        let items: Vec<ListItem> = rows
            .iter()
            .map(|(key, value)| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!(" {}", pad_display(key, 16)),
                        Style::default().fg(Color::Gray),
                    ),
                    Span::styled(value.clone(), Style::default().fg(Color::White)),
                ]))
            })
            .collect();
        let note = Line::from(Span::styled(
            "  设置会立即保存到配置文件；下载时会显示在确认页。",
            Style::default().fg(Color::DarkGray),
        ));
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" 下载设置 "),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(2)])
            .split(area);
        f.render_stateful_widget(list, chunks[0], &mut self.settings_state);
        f.render_widget(Paragraph::new(note), chunks[1]);
    }

    fn draw_modal(&self, f: &mut Frame) {
        match &self.modal {
            Modal::None => {}
            Modal::Loading(msg) => {
                let area = centered(50, 20, f.area());
                f.render_widget(Clear, area);
                let text = vec![
                    Line::from(Span::styled(
                        format!("{} {msg}", spinner(self.spin)),
                        Style::default().fg(Color::Yellow),
                    )),
                    Line::from(""),
                    Line::from(Span::styled("请稍候…", Style::default().fg(Color::DarkGray))),
                ];
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(" 请稍候 ")
                    .border_style(Style::default().fg(Color::Yellow));
                f.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: true }), area);
            }
            Modal::Message(text, error) => {
                let area = centered(70, 35, f.area());
                f.render_widget(Clear, area);
                let color = if *error { Color::Red } else { Color::Green };
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(if *error { " 出错了 " } else { " 提示 " })
                    .border_style(Style::default().fg(color));
                let lines = vec![
                    Line::from(Span::styled(text.clone(), Style::default().fg(color))),
                    Line::from(""),
                    Line::from(Span::styled(
                        "按任意键关闭",
                        Style::default().fg(Color::DarkGray),
                    )),
                ];
                f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: true }), area);
            }
            Modal::Help => {
                let area = centered(80, 85, f.area());
                f.render_widget(Clear, area);
                let lines = help_lines();
                let block = Block::default().borders(Borders::ALL).title(" 帮助 ");
                f.render_widget(
                    Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
                    area,
                );
            }
            Modal::Url(url) => {
                let area = centered(88, 40, f.area());
                f.render_widget(Clear, area);
                let lines = vec![
                    Line::from(Span::styled("直链（含 api_key，注意不要外传）", accent())),
                    Line::from(""),
                    Line::from(url.clone()),
                    Line::from(""),
                    Line::from(Span::styled(
                        "按任意键关闭",
                        Style::default().fg(Color::DarkGray),
                    )),
                ];
                let block = Block::default().borders(Borders::ALL).title(" 直链 ");
                f.render_widget(
                    Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
                    area,
                );
            }
            Modal::Confirm(name) => {
                let area = centered(50, 22, f.area());
                f.render_widget(Clear, area);
                let lines = vec![
                    Line::from(Span::styled(
                        format!("删除服务器 “{name}”？"),
                        Style::default().fg(Color::Yellow),
                    )),
                    Line::from("会移除本地保存的登录信息。"),
                    Line::from(""),
                    Line::from(Span::styled(
                        "y 确认 / n 或 Esc 取消",
                        Style::default().fg(Color::DarkGray),
                    )),
                ];
                let block = Block::default().borders(Borders::ALL).title(" 确认 ");
                f.render_widget(
                    Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
                    area,
                );
            }
            Modal::Input(im) => {
                let area = centered(60, 22, f.area());
                f.render_widget(Clear, area);
                let line = im.input.render_line("", true, false);
                let lines = vec![
                    Line::from(Span::styled(im.title.clone(), accent())),
                    Line::from(""),
                    line,
                    Line::from(""),
                    Line::from(Span::styled(
                        "Enter 保存 · Ctrl+U 清空 · Esc 取消",
                        Style::default().fg(Color::DarkGray),
                    )),
                ];
                let block = Block::default().borders(Borders::ALL).title(" 编辑 ");
                f.render_widget(
                    Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
                    area,
                );
            }
            Modal::Login(form) => {
                let area = centered(72, 65, f.area());
                f.render_widget(Clear, area);
                let mut lines = Vec::new();
                for (i, (label, input, mask)) in [
                    ("服务器地址", &form.server, false),
                    ("别名（可选）", &form.name, false),
                    ("用户名", &form.user, false),
                    ("密码", &form.password, true),
                    ("API Key", &form.api_key, true),
                ]
                .into_iter()
                .enumerate()
                {
                    let focused = form.focus == i;
                    let mut spans = vec![Span::styled(
                        format!(
                            "{} {} ",
                            if focused { "▶" } else { " " },
                            pad_display(label, 12)
                        ),
                        if focused {
                            accent()
                        } else {
                            Style::default().fg(Color::Gray)
                        },
                    )];
                    spans.extend(input.spans(mask, focused));
                    lines.push(Line::from(spans));
                }
                let insecure_focused = form.focus == 5;
                lines.push(Line::from(vec![Span::styled(
                    format!(
                        "{} [{}] 跳过证书校验（自签名）",
                        if insecure_focused { "▶" } else { " " },
                        if form.insecure { "x" } else { " " }
                    ),
                    if insecure_focused {
                        accent()
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                )]));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Tab/Enter 下一项 · Ctrl+S 提交 · Esc 取消",
                    Style::default().fg(Color::DarkGray),
                )));
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(" 登录服务器 ");
                f.render_widget(
                    Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
                    area,
                );
            }
            Modal::Seasons(ss) => {
                let area = centered(55, 72, f.area());
                f.render_widget(Clear, area);
                let mut lines = Vec::new();
                for (i, (season, on)) in ss.seasons.iter().enumerate() {
                    let selected = ss.state.selected() == Some(i);
                    let label = if *season == 0 {
                        "特别篇 (Season 00)".to_string()
                    } else {
                        format!("Season {:02}", season)
                    };
                    lines.push(Line::from(vec![Span::styled(
                        format!("{} [{}] {label}", if selected { "›" } else { " " }, if *on { "x" } else { " " }),
                        if selected {
                            accent()
                        } else {
                            Style::default()
                        },
                    )]));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "↑↓ 选择 · 空格 勾选 · a 全选/全不选 · Enter 确认 · Esc 返回",
                    Style::default().fg(Color::DarkGray),
                )));
                let block = Block::default().borders(Borders::ALL).title(" 选择季 ");
                f.render_widget(Paragraph::new(lines).block(block), area);
            }
            Modal::Detail => {
                if let Some(d) = &self.detail {
                    self.draw_detail(f, d);
                }
            }
        }
    }

    fn draw_detail(&self, f: &mut Frame, d: &Detail) {
        let area = centered(90, 85, f.area());
        f.render_widget(Clear, area);

        let mut lines = Vec::new();
        lines.push(Line::from(vec![
            Span::styled(d.root.title(), accent()),
            Span::styled(
                format!("  {}", d.root.display_type()),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        let total = self
            .detail_total_size(d)
            .map(format_bytes)
            .unwrap_or_else(|| "-".into());
        lines.push(Line::from(format!(
            "服务器：{}    条目：{}    预估大小：{}",
            d.server,
            d.files.len(),
            total
        )));
        lines.push(Line::from(""));

        if d.versions.is_empty() {
            lines.push(Line::from(Span::styled(
                "版本：单一版本",
                Style::default().fg(Color::Gray),
            )));
        } else if d.versions.len() == 1 {
            let v = &d.versions[0];
            let size = v.size.map(format_bytes).unwrap_or_else(|| "-".into());
            let count = if v.is_episode {
                format!("{} 集", v.count)
            } else {
                format!("{} 个", v.count)
            };
            lines.push(Line::from(vec![
                Span::styled("版本：", Style::default().fg(Color::Gray)),
                Span::styled(v.label.clone(), Style::default().fg(Color::White)),
                Span::styled(
                    format!("  {count}  {size}  {} {}", v.res, v.container),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        } else {
            for (i, v) in d.versions.iter().enumerate() {
                let chosen = matches!(d.choice, VersionChoice::One(j) if j == i);
                let size = v.size.map(format_bytes).unwrap_or_else(|| "-".into());
                let count = if v.is_episode {
                    format!("{} 集", v.count)
                } else {
                    format!("{} 个", v.count)
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  [{}] {} ", i + 1, if chosen { "●" } else { "○" }),
                        if chosen {
                            Style::default().fg(Color::Green)
                        } else {
                            Style::default().fg(Color::Gray)
                        },
                    ),
                    Span::styled(
                        pad_display(&v.label, 28),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled(
                        format!("{count:>8}  {size:>10}  {} {}", v.res, v.container),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            let all = matches!(d.choice, VersionChoice::All);
            lines.push(Line::from(Span::styled(
                format!("  [0] {} 全部版本（每个版本分别保存）", if all { "●" } else { "○" }),
                if all {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Gray)
                },
            )));
        }

        if d.files.len() == 1
            && d.versions.len() <= 1
            && let Some(it) = d.files.first()
        {
            let (size, res, video, audio, container) = crate::file_info(it);
            let mut parts = Vec::new();
            if !res.is_empty() {
                parts.push(res);
            }
            if !video.is_empty() {
                parts.push(format!("视频 {video}"));
            }
            if !audio.is_empty() {
                parts.push(format!("音频 {audio}"));
            }
            if !container.is_empty() {
                parts.push(container);
            }
            if let Some(s) = size {
                parts.push(format_bytes(s));
            }
            if !parts.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("媒体：{}", parts.join(" · ")),
                    Style::default().fg(Color::Gray),
                )));
            }
            if let Some(path) = &it.path {
                lines.push(Line::from(Span::styled(
                    format!("路径：{path}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }

        let seasons = if d.seasons.is_empty() {
            "季：-".to_string()
        } else if d.seasons.iter().all(|(_, on)| *on) {
            "季：全部（按 f 修改）".to_string()
        } else {
            let picked: Vec<String> = d
                .seasons
                .iter()
                .filter(|(_, on)| *on)
                .map(|(s, _)| s.to_string())
                .collect();
            format!(
                "季：{}（按 f 修改）",
                if picked.is_empty() {
                    "未选择".to_string()
                } else {
                    picked.join(", ")
                }
            )
        };
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(seasons, Style::default().fg(Color::Gray))));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "输出：{}    连接：{}    每服并发：{}    限速：{}    字幕：{}    覆盖：{}",
                self.settings.out,
                self.settings.connections,
                self.settings.jobs,
                if self.settings.limit.trim().is_empty() {
                    "不限".to_string()
                } else {
                    self.settings.limit.clone()
                },
                on_off(self.settings.subs),
                if self.settings.overwrite { "是" } else { "否" }
            ),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(""));
        let hint = if d.versions.len() > 1 {
            "1-9 选版本 · 0/a 全部版本 · f 选择季 · u 查看直链 · Enter 开始下载 · Esc 取消"
        } else {
            "f 选择季 · u 查看直链 · Enter 开始下载 · Esc 取消"
        };
        lines.push(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::Yellow),
        )));

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" 下载确认 ");
        f.render_widget(
            Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
            area,
        );
    }
}

// ───────────────────────── 输入框 ─────────────────────────

#[derive(Default, Clone)]
struct TextInput {
    text: String,
    /// 光标位置（按字符计）
    cursor: usize,
}

impl TextInput {
    fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            cursor: text.chars().count(),
        }
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }

    fn insert(&mut self, c: char) {
        let b = self.byte_at(self.cursor);
        self.text.insert(b, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let b = self.byte_at(self.cursor - 1);
            self.text.remove(b);
            self.cursor -= 1;
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.len() {
            let b = self.byte_at(self.cursor);
            self.text.remove(b);
        }
    }

    fn handle(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.text.clear();
                self.cursor = 0;
                true
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert(c);
                true
            }
            KeyCode::Backspace => {
                self.backspace();
                true
            }
            KeyCode::Delete => {
                self.delete();
                true
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                true
            }
            KeyCode::Right => {
                if self.cursor < self.len() {
                    self.cursor += 1;
                }
                true
            }
            KeyCode::Home => {
                self.cursor = 0;
                true
            }
            KeyCode::End => {
                self.cursor = self.len();
                true
            }
            _ => false,
        }
    }

    fn parts(&self, mask: bool) -> (String, String, String) {
        let chars: Vec<char> = self.text.chars().collect();
        let cur = self.cursor.min(chars.len());
        let conv = |slice: &[char]| {
            if mask {
                "•".repeat(slice.len())
            } else {
                slice.iter().collect()
            }
        };
        let before = conv(&chars[..cur]);
        let at = chars
            .get(cur)
            .map(|c| if mask { '•' } else { *c })
            .unwrap_or(' ');
        let after = conv(&chars[(cur + 1).min(chars.len())..]);
        (before, at.to_string(), after)
    }

    fn spans(&self, mask: bool, focused: bool) -> Vec<Span<'static>> {
        let (before, at, after) = self.parts(mask);
        let cursor_style = if focused {
            Style::default().bg(Color::Cyan).fg(Color::Black)
        } else {
            Style::default()
        };
        vec![
            Span::raw(before),
            Span::styled(at, cursor_style),
            Span::raw(after),
        ]
    }

    fn render_line(&self, label: &str, focused: bool, mask: bool) -> Line<'static> {
        let mut spans = Vec::new();
        if !label.is_empty() {
            spans.push(Span::styled(
                label.to_string(),
                Style::default().fg(Color::Gray),
            ));
        }
        spans.extend(self.spans(mask, focused));
        Line::from(spans)
    }
}

// ───────────────────────── 小工具 ─────────────────────────

fn accent() -> Style {
    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
}

fn spinner(i: usize) -> char {
    ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'][i % 10]
}

fn on_off(v: bool) -> String {
    if v { "开".into() } else { "关".into() }
}

/// 按显示宽度补空格（中文占 2 列）。
fn pad_display(s: &str, width: usize) -> String {
    let w = s.width();
    format!("{s}{}", " ".repeat(width.saturating_sub(w)))
}

fn move_sel(state: &mut ListState, len: usize, delta: isize) {
    if len == 0 {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0) as isize;
    let next = (cur + delta).clamp(0, len as isize - 1);
    state.select(Some(next as usize));
}

fn selected_server(cfg: &Config, idx: Option<usize>) -> Option<String> {
    let idx = idx?;
    cfg.servers.keys().nth(idx).cloned()
}

fn year_label(item: &Item) -> String {
    item.production_year
        .filter(|y| *y >= 1800)
        .map(|y| y.to_string())
        .unwrap_or_else(|| "-".into())
}

fn season_label(item: &Item) -> String {
    if item.item_type.as_deref() == Some("Episode") {
        format!("S{:02}", item.season_no())
    } else {
        "-".into()
    }
}

fn type_tag(item: &Item) -> &'static str {
    match item.item_type.as_deref() {
        Some("Series") => "剧",
        Some("Season") => "季",
        Some("Episode") => "集",
        Some("Movie") => "影",
        Some("Audio") => "音",
        Some("MusicAlbum") | Some("MusicArtist") => "乐",
        Some("Folder") | Some("CollectionFolder") | Some("BoxSet") | Some("Playlist") => "夹",
        _ => "·",
    }
}

fn item_meta(item: &Item) -> String {
    let mut parts = Vec::new();
    if let Some(y) = item.production_year.filter(|y| *y >= 1800) {
        parts.push(y.to_string());
    }
    if item.item_type.as_deref() == Some("Episode") {
        parts.push(format!("S{:02}E{:02}", item.season_no(), item.episode_no()));
    }
    if let Some(s) = item.size() {
        parts.push(format_bytes(s));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("  {}", parts.join(" · "))
    }
}

fn progress_bar(frac: f64, width: usize) -> String {
    let filled = (frac.clamp(0.0, 1.0) * width as f64).round() as usize;
    format!(
        "{}{}",
        "█".repeat(filled),
        "░".repeat(width.saturating_sub(filled))
    )
}

fn centered(px: u16, py: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - py) / 2),
            Constraint::Percentage(py),
            Constraint::Percentage((100 - py) / 2),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - px) / 2),
            Constraint::Percentage(px),
            Constraint::Percentage((100 - px) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
}

fn help_lines() -> Vec<Line<'static>> {
    let key = |k: &str, desc: &str| {
        Line::from(vec![
            Span::styled(format!("  {k:<16}"), Style::default().fg(Color::Cyan)),
            Span::raw(desc.to_string()),
        ])
    };
    vec![
        Line::from(Span::styled("页面", accent())),
        key("1-5 / Tab", "切换 搜索 / 浏览 / 下载 / 服务器 / 设置"),
        key("Shift+Tab", "向前切换页面"),
        key("q", "退出（输入框内不生效）"),
        key("Ctrl+C", "随时退出"),
        key("?", "显示本帮助"),
        Line::from(""),
        Line::from(Span::styled("搜索", accent())),
        key("Enter", "开始搜索（所有服务器或当前范围；输入 http(s) 直链直接下载）"),
        key("Tab / ↓", "进入结果列表（Tab 也可切页）"),
        key("↑↓", "在结果中移动；顶部再按 ↑ 回到输入框"),
        key("Enter / d", "打开条目详情（解析版本、季）"),
        key("/", "回到关键字输入框（结果为空时按 Enter 也可以）"),
        key("Esc", "清空关键字"),
        Line::from(""),
        Line::from(Span::styled("浏览", accent())),
        key("Enter", "进入目录 / 打开可播放条目"),
        key("d", "直接打开当前条目的下载确认页"),
        key("Backspace", "返回上一层"),
        key("r", "重新加载当前层"),
        Line::from(""),
        Line::from(Span::styled("下载队列", accent())),
        key("↑↓", "选择服务器"),
        key("x / X", "取消当前服务器的队列 / 取消全部"),
        key("c", "清空最近完成记录"),
        Line::from(Span::styled(
            "  下载在后台排队进行，每个服务器一条独立下载流，不影响继续操作。",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled("服务器", accent())),
        key("a", "添加服务器（账号密码或 API Key）"),
        key("d", "删除当前服务器"),
        key("Enter", "设为搜索范围（再按一次恢复全部）"),
        key("l", "用该服务器打开「浏览」"),
        Line::from(""),
        Line::from(Span::styled("下载确认", accent())),
        key("1-9", "选择对应版本"),
        key("0 / a", "下载全部版本"),
        key("f", "选择要下载的季"),
        key("u", "查看直链（含 api_key）"),
        key("Enter", "加入后台下载队列（界面保持可用）"),
        Line::from(""),
        Line::from(Span::styled("设置", accent())),
        key("←→", "调整数字 / 开关"),
        key("Enter", "编辑输出目录、限速、命名模板（Ctrl+U 清空）"),
        Line::from(""),
        Line::from(Span::styled(
            " 按任意键关闭帮助",
            Style::default().fg(Color::DarkGray),
        )),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn text_input_edits_by_char() {
        let mut input = TextInput::default();
        for c in "庆余年".chars() {
            input.insert(c);
        }
        assert_eq!(input.text, "庆余年");
        assert_eq!(input.cursor, 3);
        input.backspace();
        assert_eq!(input.text, "庆余");
        input.cursor = 1;
        input.insert('a');
        assert_eq!(input.text, "庆a余");
        assert_eq!(input.cursor, 2);
        input.delete();
        assert_eq!(input.text, "庆a");
    }

    #[test]
    fn text_input_handles_keys() {
        let mut input = TextInput::new("abc");
        input.handle(&key(KeyCode::Left));
        input.handle(&key(KeyCode::Backspace));
        assert_eq!(input.text, "ac");
        assert_eq!(input.cursor, 1);
        input.handle(&key(KeyCode::Home));
        assert_eq!(input.cursor, 0);
        input.handle(&key(KeyCode::Delete));
        assert_eq!(input.text, "c");
    }

    #[test]
    fn masked_parts_do_not_leak_text() {
        let input = TextInput::new("secret");
        let (before, at, after) = input.parts(true);
        let shown = format!("{before}{at}{after}");
        assert_eq!(shown, "•••••• ");
        assert!(!shown.contains('s'));
    }
}
