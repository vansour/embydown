//! 命令行交互模式：不带子命令运行 `embydown`（或 `embydown ui`）进入。
//!
//! 全部使用数字菜单与逐行输入，不依赖任何全屏终端库。下载仍交给后台
//! 下载服务执行，因此退出交互模式或关闭终端不会中断已经入队的任务。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::cli::LoginArgs;
use crate::config::{self, Config, Profile, Settings};
use crate::daemon::DaemonHandle;
use crate::download::DownloadOpts;
use crate::emby::Client;
use crate::manager::FinishedStatus;
use crate::model::Item;
use crate::repair;
use crate::table::{Align, Table};
use crate::util::{format_bytes, parse_rate};
use crate::{
    expand_versions, filter_to_version, has_multiple_versions, resolve_item, version_summaries,
};

const SEARCH_LIMIT: u32 = 30;
const DIRECT_SERVER: &str = "（直链）";
const QUEUE_PREVIEW: usize = 20;

/// 进入命令行交互模式。
pub async fn run() -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!("交互模式需要在终端中运行；脚本请使用子命令，例如 embydown dl <关键字> --dry-run");
    }
    let mut app = App::load().await;
    // 给后台服务一点时间返回第一份状态快照。
    tokio::time::sleep(Duration::from_millis(300)).await;
    app.main_menu().await
}

struct App {
    cfg: Config,
    clients: BTreeMap<String, Client>,
    daemon: DaemonHandle,
    /// 搜索范围（服务器别名）；None 表示全部服务器。
    scope: Option<String>,
}

impl App {
    async fn load() -> Self {
        let cfg = config::load_optional().ok().flatten().unwrap_or_default();
        let mut app = Self {
            cfg,
            clients: BTreeMap::new(),
            daemon: DaemonHandle::new(),
            scope: None,
        };
        app.reconnect().await;
        app
    }

    async fn reconnect(&mut self) {
        let servers: Vec<(String, Profile)> = self
            .cfg
            .servers
            .iter()
            .map(|(name, profile)| (name.clone(), profile.clone()))
            .collect();
        self.clients.clear();
        for (name, profile) in servers {
            match crate::client_for(&name, &profile).await {
                Ok(client) => {
                    self.clients.insert(name, client);
                }
                Err(e) => println!("⚠️  {name} 连接失败：{e:#}"),
            }
        }
        if let Some(name) = &self.scope
            && !self.cfg.servers.contains_key(name)
        {
            self.scope = None;
        }
    }

    async fn reload_config(&mut self) {
        self.cfg = config::load_optional().ok().flatten().unwrap_or_default();
        self.reconnect().await;
    }

    fn print_header(&self) {
        print_rule(60);
        println!(
            " {}  {}",
            bold("embydown"),
            dim(&format!("v{}", env!("CARGO_PKG_VERSION")))
        );
        let servers = if self.cfg.servers.is_empty() {
            "未登录".to_string()
        } else {
            format!(
                "{} 个（{}）",
                self.cfg.servers.len(),
                truncate_display(&self.cfg.name_list(), 46)
            )
        };
        print_key_value("服务器", &servers);
        print_key_value("搜索范围", self.scope.as_deref().unwrap_or("全部服务器"));
        let temp = self.cfg.settings.temp_dir.trim();
        let temp = if temp.is_empty() { "./downloads" } else { temp };
        print_key_value("临时文件夹", &truncate_display(temp, 52));
        let final_dir = self.cfg.settings.final_dir.trim();
        let final_dir = if final_dir.is_empty() {
            "（未设置）"
        } else {
            final_dir
        };
        print_key_value("最终文件夹", &truncate_display(final_dir, 52));
        let queue = self.queue_summary();
        print_key_value("下载服务", &queue);
        print_rule(60);
    }

    fn queue_summary(&self) -> String {
        let state = self.daemon.state();
        if state.servers.is_empty() {
            return "空闲".to_string();
        }
        let active: usize = state.servers.values().map(|server| server.active.len()).sum();
        let queued: usize = state.servers.values().map(|server| server.queue.len()).sum();
        let paused = state
            .servers
            .values()
            .filter(|server| server.paused)
            .count();
        let mut parts = vec![format!("{active} 下载中"), format!("{queued} 排队")];
        if paused > 0 {
            parts.push(format!("{paused} 已暂停"));
        }
        parts.join(" · ")
    }

    async fn main_menu(&mut self) -> Result<()> {
        loop {
            self.print_header();
            println!();
            println!("{} {}", cyan("▸"), bold("主菜单"));
            print_options(&[
                ("1)", "搜索并下载"),
                ("2)", "浏览媒体库"),
                ("3)", "下载队列"),
                ("4)", "服务器管理"),
                ("5)", "下载设置"),
                ("6)", "未完成任务找回"),
                ("7)", "下载记录"),
                ("8)", "本地审查"),
                ("9)", "同步到最终文件夹"),
                ("10)", "秒传测试"),
                ("0)", "退出"),
            ]);
            println!();
            println!(
                "{}",
                dim("输入编号后回车；0 返回/退出，Ctrl+C 随时退出。")
            );

            let Some(choice) = prompt("选择 [0-10]：")? else {
                break;
            };
            match choice.as_str() {
                "1" => run_step(self.search_menu()).await,
                "2" => run_step(self.browse_menu()).await,
                "3" => run_step(self.queue_menu()).await,
                "4" => run_step(self.servers_menu()).await,
                "5" => run_step(self.settings_menu()).await,
                "6" => run_step(self.unfinished_menu()).await,
                "7" => run_step(self.history_menu()).await,
                "8" => {
                    if let Err(e) = crate::review::run(None, None) {
                        println!("{} {e:#}", red("✗"));
                    }
                }
                "9" => {
                    if let Err(e) = self.sync_menu() {
                        println!("{} {e:#}", red("✗"));
                    }
                }
                "10" => run_step(self.instant_test_menu()).await,
                "" => {}
                "0" | "q" => break,
                _ => println!("{} 无效选择", yellow("!")),
            }
            if let Some(err) = self.daemon.take_error() {
                println!("{} {err}", yellow("!"));
            }
            println!();
        }
        println!();
        println!("{} 已退出交互模式。", dim("·"));
        Ok(())
    }

    // ───────────────────────── 搜索 ─────────────────────────

    async fn search_menu(&mut self) -> Result<()> {
        print_section("搜索并下载");
        let Some((server, item)) = self.select_source().await? else {
            return Ok(());
        };
        self.prepare_download(server, item).await
    }

    /// 搜索并让用户选择影片/来源；下载与秒传测试共用。
    async fn select_source(&mut self) -> Result<Option<(String, Item)>> {
        println!(
            "{}",
            dim("可输入影片/剧集关键字或 http(s) 直链；直接回车返回主菜单。")
        );
        let Some(term) = ask("搜索关键字：")? else {
            return Ok(None);
        };
        if term.starts_with("http://") || term.starts_with("https://") {
            return Ok(Some((DIRECT_SERVER.to_string(), crate::direct_item(&term))));
        }

        let targets = self.search_targets();
        if targets.is_empty() {
            println!(
                "{} 没有可用的服务器，请先在「服务器管理」中登录",
                yellow("!")
            );
            return Ok(None);
        }
        println!(
            "{} 正在 {} 个服务器搜索 “{term}”…",
            dim("·"),
            targets.len()
        );
        let mut hits = Vec::new();
        let mut errors = Vec::new();
        for (name, client) in &targets {
            match client.search(&term, SEARCH_LIMIT).await {
                Ok(items) => hits.extend(items.into_iter().map(|item| Hit {
                    server: name.clone(),
                    item,
                })),
                Err(e) => errors.push(format!("{name}: {e:#}")),
            }
        }
        for error in &errors {
            println!("{} {error}", yellow("!"));
        }
        if hits.is_empty() {
            println!("{} 没有匹配结果", yellow("!"));
            return Ok(None);
        }

        let groups = group_hits(hits, &term);
        let sources: usize = groups.iter().map(|g| g.hits.len()).sum();
        println!();
        println!(
            "{} 找到 {} 个影片，共 {} 个来源",
            green("✓"),
            bold(&groups.len().to_string()),
            sources
        );
        print_groups(&groups);
        let Some(idx) = choose(groups.len(), "选择影片编号（回车取消）：")? else {
            return Ok(None);
        };
        let group = &groups[idx];
        let options = source_options(group);
        println!();
        println!("{} {}", cyan("▸"), bold(&format!("{} 的来源", group.title)));
        print_sources(&options);
        let Some(sidx) = choose(options.len(), "选择来源编号（回车取消）：")? else {
            return Ok(None);
        };
        let source = &options[sidx];
        Ok(Some((source.server.clone(), source.item.clone())))
    }

    async fn instant_test_menu(&mut self) -> Result<()> {
        print_section("秒传测试");
        println!(
            "{}",
            dim("只下载第一集并移动到最终文件夹，通过 CD2 上传任务判断是否秒传。")
        );
        if self.cfg.settings.final_dir.trim().is_empty() {
            println!(
                "{} 请先在「下载设置」中设置最终文件夹",
                yellow("!")
            );
            return Ok(());
        }
        let Some((server, item)) = self.select_source().await? else {
            return Ok(());
        };
        let client = if server == DIRECT_SERVER {
            Client::new("http://127.0.0.1", false).context("构建直链下载客户端失败")?
        } else {
            self.clients
                .get(&server)
                .cloned()
                .with_context(|| format!("服务器 {server} 未连接"))?
        };

        let cd2 = crate::cd2::Config::default();
        println!();
        print_key_value("服务器", &server);
        print_key_value("CD2 API", &cd2.url);
        if cd2.token.is_none() {
            println!(
                "  {}",
                dim("未设置 CD2 令牌；如果 CD2 开启鉴权，请先设置 CLOUDDRIVE_API_TOKEN。")
            );
        }
        if !confirm("开始秒传测试？", true)? {
            println!("{} 已取消", yellow("!"));
            return Ok(());
        }
        let keep = confirm("测试后保留最终文件夹中的测试目录？", false)?;
        let options = crate::instant::Options {
            cd2,
            window: Duration::from_secs(20),
            keep,
            temp_dir: None,
            final_dir: None,
        };
        crate::instant::run_item(server, client, item, &self.cfg.settings, options).await
    }

    fn search_targets(&self) -> Vec<(String, Client)> {        match &self.scope {
            Some(name) => match self.clients.get(name) {
                Some(client) => vec![(name.clone(), client.clone())],
                None => {
                    println!("{} 搜索范围 {name} 当前未连接，已改为搜索全部服务器", yellow("!"));
                    self.clients
                        .iter()
                        .map(|(n, c)| (n.clone(), c.clone()))
                        .collect()
                }
            },
            None => self
                .clients
                .iter()
                .map(|(n, c)| (n.clone(), c.clone()))
                .collect(),
        }
    }

    /// 解析条目、选择季/版本，确认后加入后台下载队列。
    async fn prepare_download(&mut self, server: String, root: Item) -> Result<()> {
        let client = self.client_for_server(&server)?;
        let mut files = if root.is_container() {
            println!("{} 正在解析 “{}” …", dim("·"), root.title());
            resolve_item(&client, root.clone(), &[])
                .await
                .with_context(|| format!("解析 {} 失败", root.title()))?
        } else {
            vec![root.clone()]
        };
        if files.is_empty() {
            println!("{} 没有可下载的条目", yellow("!"));
            return Ok(());
        }

        let Some(kept) = choose_seasons(files)? else {
            println!("{} 已取消", yellow("!"));
            return Ok(());
        };
        files = kept;
        if files.is_empty() {
            println!("{} 所选季下没有可下载的条目", yellow("!"));
            return Ok(());
        }

        let Some(kept) = choose_versions(files)? else {
            println!("{} 已取消", yellow("!"));
            return Ok(());
        };
        files = kept;
        if files.is_empty() {
            println!("{} 所选版本下没有可下载的条目", yellow("!"));
            return Ok(());
        }

        let opts = build_opts(&self.cfg.settings)?;
        let sizes: Vec<u64> = files.iter().filter_map(Item::size).collect();
        let total = if sizes.is_empty() {
            "-".to_string()
        } else {
            format_bytes(sizes.iter().sum())
        };
        println!();
        print_rule(60);
        print_key_value("服务器", &server);
        print_key_value("文件", &format!("{} 个，共 {total}", files.len()));
        print_key_value("临时文件夹", &opts.out.display().to_string());
        print_rule(60);
        if !confirm("加入后台下载队列？", true)? {
            println!("{} 已取消", yellow("!"));
            return Ok(());
        }
        self.daemon.enqueue(server.clone(), client, files, opts);
        println!(
            "{} 已加入 {} 的下载队列；可在「下载队列」中查看进度。",
            green("✓"),
            server
        );
        Ok(())
    }

    fn client_for_server(&self, server: &str) -> Result<Client> {
        if server == DIRECT_SERVER {
            Client::new("http://127.0.0.1", false).context("构建直链下载客户端失败")
        } else {
            self.clients
                .get(server)
                .cloned()
                .with_context(|| format!("服务器 {server} 未连接"))
        }
    }

    // ───────────────────────── 浏览 ─────────────────────────

    async fn browse_menu(&mut self) -> Result<()> {
        let Some(server) = self.choose_server("选择服务器 [1-N]（回车取消）：")? else {
            return Ok(());
        };
        let client = self
            .clients
            .get(&server)
            .cloned()
            .with_context(|| format!("服务器 {server} 未连接"))?;

        let views = client.views().await.context("获取媒体库列表失败")?;
        let mut stack: Vec<(Option<Item>, String, Vec<Item>)> =
            vec![(None, "媒体库".to_string(), views)];
        loop {
            let title = stack.last().map(|l| l.1.clone()).unwrap_or_default();
            print_section(&format!("浏览 · {server}"));
            println!("  当前目录：{}", bold(&title));
            {
                let items = &stack.last().expect("浏览栈不为空").2;
                if items.is_empty() {
                    println!("  {}", dim("（空目录）"));
                } else {
                    let mut table =
                        Table::new(&["#", "类型", "名称", "信息"]).align(0, Align::Right);
                    for (i, item) in items.iter().enumerate() {
                        table.row(vec![
                            (i + 1).to_string(),
                            type_label(item).to_string(),
                            item.title(),
                            item_meta(item).trim().to_string(),
                        ]);
                    }
                    println!("{}", table.render());
                }
            }
            print_options(&[
                ("0)", "返回上一层"),
                ("d)", "下载当前目录"),
                ("q)", "退出浏览"),
            ]);

            let Some(input) = prompt("选择编号：")? else {
                return Ok(());
            };
            match input.as_str() {
                "0" => {
                    if stack.len() > 1 {
                        stack.pop();
                    } else {
                        return Ok(());
                    }
                }
                "q" | "" => return Ok(()),
                "d" => {
                    let parent = stack.last().and_then(|l| l.0.clone());
                    match parent {
                        Some(item) => self.prepare_download(server.clone(), item).await?,
                        None => println!("已经在媒体库根目录，请进入一个库后再下载"),
                    }
                }
                _ => {
                    let Ok(n) = input.parse::<usize>() else {
                        println!("{} 无效输入", yellow("!"));
                        continue;
                    };
                    let selected = {
                        let items = &stack.last().expect("浏览栈不为空").2;
                        if n == 0 || n > items.len() {
                            None
                        } else {
                            Some(items[n - 1].clone())
                        }
                    };
                    let Some(item) = selected else {
                        println!("{} 编号超出范围", yellow("!"));
                        continue;
                    };
                    if item.is_container() {
                        println!("{} 加载 “{}” …", dim("·"), item.title());
                        match client.children(&item.id, false, None, None).await {
                            Ok(children) => {
                                stack.push((Some(item.clone()), item.title(), children));
                            }
                            Err(e) => println!("加载失败：{e:#}"),
                        }
                    } else {
                        self.prepare_download(server.clone(), item).await?;
                    }
                }
            }
        }
    }

    // ───────────────────────── 下载队列 ─────────────────────────

    async fn queue_menu(&mut self) -> Result<()> {
        // 给后台连接任务一点时间拿到第一份快照。
        tokio::time::sleep(Duration::from_millis(300)).await;
        print_section("下载队列");
        loop {
            self.print_queue();
            println!();
            print_options(&[
                ("r)", "刷新"),
                ("p)", "暂停/继续"),
                ("P)", "全部暂停/继续"),
                ("x)", "取消服务器队列"),
                ("X)", "取消全部"),
                ("c)", "清空完成记录"),
                ("u)", "未完成任务找回"),
                ("0)", "返回"),
            ]);
            let Some(choice) = prompt("请选择：")? else {
                return Ok(());
            };
            match choice.as_str() {
                "r" | "" => {}
                "p" => {
                    let Some(server) = self.choose_queue_server("暂停/继续哪个服务器 [1-N]：")?
                    else {
                        continue;
                    };
                    let paused = {
                        let state = self.daemon.state();
                        state
                            .servers
                            .get(&server)
                            .map(|s| s.paused)
                            .unwrap_or(false)
                    };
                    if paused {
                        self.daemon.resume_server(&server);
                        println!("{} 已继续 {server}", green("✓"));
                    } else {
                        self.daemon.pause_server(&server);
                        println!("{} 已暂停 {server}", yellow("!"));
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                "P" => {
                    let all_paused = {
                        let state = self.daemon.state();
                        !state.servers.is_empty() && state.servers.values().all(|s| s.paused)
                    };
                    if all_paused {
                        self.daemon.resume_all();
                        println!("{} 已继续所有服务器的下载", green("✓"));
                    } else {
                        self.daemon.pause_all();
                        println!("{} 已暂停所有服务器的下载", yellow("!"));
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                "x" => {
                    let Some(server) = self.choose_queue_server("取消哪个服务器的队列 [1-N]：")?
                    else {
                        continue;
                    };
                    self.daemon.cancel_server(&server);
                    println!("{} 已取消 {server} 的队列", green("✓"));
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                "X" => {
                    if confirm("取消所有服务器的下载队列？", false)? {
                        self.daemon.cancel_all();
                        println!("{} 已取消所有服务器的下载", green("✓"));
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                }
                "c" => {
                    if confirm("清空最近完成记录并删除历史文件？", false)? {
                        self.daemon.clear_finished();
                        println!("{} 已清空下载记录", green("✓"));
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                }
                "u" => self.unfinished_menu().await?,
                "0" | "q" => return Ok(()),
                _ => println!("无效选择"),
            }
        }
    }

    fn print_queue(&self) {
        let state = self.daemon.state();
        if state.servers.is_empty() && state.finished.is_empty() {
            println!("  {}", dim("（暂无下载任务）"));
            return;
        }

        if !state.servers.is_empty() {
            let mut table = Table::new(&[
                "服务器", "状态", "运行", "排队", "完成", "失败", "跳过", "取消",
            ])
            .align(2, Align::Right)
            .align(3, Align::Right)
            .align(4, Align::Right)
            .align(5, Align::Right)
            .align(6, Align::Right)
            .align(7, Align::Right);
            for (name, server) in &state.servers {
                table.row(vec![
                    name.clone(),
                    if server.paused { "已暂停" } else { "运行中" }.to_string(),
                    server.active.len().to_string(),
                    server.queue.len().to_string(),
                    server.done.to_string(),
                    server.failed.to_string(),
                    server.skipped.to_string(),
                    server.cancelled.to_string(),
                ]);
            }
            println!("{}", table.render());
        }

        let active: Vec<(String, String, String, String, String)> = state
            .servers
            .iter()
            .flat_map(|(name, server)| {
                server.active.iter().map(move |job| {
                    let pct = if job.length > 0 {
                        job.position as f64 / job.length as f64 * 100.0
                    } else {
                        0.0
                    };
                    (
                        name.clone(),
                        format!("{pct:.1}%"),
                        format_bytes(job.position),
                        format_bytes(job.length),
                        job.title.clone(),
                    )
                })
            })
            .collect();
        if !active.is_empty() {
            println!();
            println!("{}", bold("正在下载"));
            let mut table = Table::new(&["服务器", "进度", "已下载", "总大小", "名称"])
                .align(1, Align::Right)
                .align(2, Align::Right)
                .align(3, Align::Right);
            for (server, pct, position, length, title) in active {
                table.row(vec![server, pct, position, length, title]);
            }
            println!("{}", table.render());
        }

        let queued: Vec<(String, String)> = state
            .servers
            .iter()
            .flat_map(|(name, server)| {
                server
                    .queue
                    .iter()
                    .map(move |job| (name.clone(), job.title.clone()))
            })
            .collect();
        if !queued.is_empty() {
            println!();
            println!("{}", bold("排队任务"));
            let mut table = Table::new(&["服务器", "名称"]);
            for (server, title) in queued.iter().take(QUEUE_PREVIEW) {
                table.row(vec![server.clone(), title.clone()]);
            }
            println!("{}", table.render());
            if queued.len() > QUEUE_PREVIEW {
                println!(
                    "  {}",
                    dim(&format!(
                        "… 还有 {} 个排队任务（只显示前 {QUEUE_PREVIEW} 个）",
                        queued.len() - QUEUE_PREVIEW
                    ))
                );
            }
        }

        if !state.finished.is_empty() {
            println!();
            println!("{}", bold("最近完成"));
            let mut table = Table::new(&["状态", "服务器", "名称"]);
            let mut failures = Vec::new();
            for done in state.finished.iter().take(10) {
                let status = match done.status {
                    FinishedStatus::Done => "完成",
                    FinishedStatus::Skipped => "跳过",
                    FinishedStatus::Failed => "失败",
                    FinishedStatus::Cancelled => "取消",
                };
                if done.status == FinishedStatus::Failed
                    && let Some(error) = &done.error
                {
                    let error = error.lines().next().unwrap_or(error);
                    failures.push(format!(
                        "{}：{}",
                        done.title,
                        truncate_display(error, 60)
                    ));
                }
                table.row(vec![
                    status.to_string(),
                    done.server.clone(),
                    done.title.clone(),
                ]);
            }
            println!("{}", table.render());
            if !failures.is_empty() {
                println!("  {}", dim("失败原因："));
                for failure in failures.iter().take(3) {
                    println!("    {} {failure}", dim("↳"));
                }
                if failures.len() > 3 {
                    println!("    {}", dim(&format!("… 还有 {} 条", failures.len() - 3)));
                }
            }
        }
    }

    fn choose_queue_server(&self, hint: &str) -> Result<Option<String>> {
        let names: Vec<String> = {
            let state = self.daemon.state();
            state.servers.keys().cloned().collect()
        };
        choose_name(&names, hint)
    }

    // ───────────────────────── 服务器 ─────────────────────────

    async fn servers_menu(&mut self) -> Result<()> {
        loop {
            print_section("服务器管理");
            if self.cfg.servers.is_empty() {
                println!("  {}", dim("（尚未登录）"));
            } else {
                let mut table = Table::new(&["#", "名称", "地址", "状态", "搜索范围"])
                    .align(0, Align::Right);
                for (i, (name, profile)) in self.cfg.servers.iter().enumerate() {
                    let connected = if self.clients.contains_key(name) {
                        "已连接"
                    } else {
                        "未连接"
                    };
                    let scope = if self.scope.as_deref() == Some(name.as_str()) {
                        "当前"
                    } else {
                        "-"
                    };
                    table.row(vec![
                        (i + 1).to_string(),
                        name.clone(),
                        profile.server.clone(),
                        connected.to_string(),
                        scope.to_string(),
                    ]);
                }
                println!("{}", table.render());
            }
            println!(
                "  当前搜索范围：{}",
                bold(self.scope.as_deref().unwrap_or("全部服务器"))
            );
            print_options(&[
                ("a)", "添加/登录"),
                ("d)", "删除"),
                ("s)", "设置搜索范围"),
                ("0)", "返回"),
            ]);

            let Some(choice) = prompt("请选择：")? else {
                return Ok(());
            };
            match choice.as_str() {
                "a" => {
                    if self.login_flow().await? {
                        self.reload_config().await;
                    }
                }
                "d" => {
                    let Some(name) = ask("要删除的服务器名称（回车取消）：")? else {
                        continue;
                    };
                    if !self.cfg.servers.contains_key(&name) {
                        println!("{} 没有名为 {name} 的服务器", yellow("!"));
                        continue;
                    }
                    if !confirm(&format!("删除 {name} 的登录信息？"), false)? {
                        continue;
                    }
                    match crate::logout(Some(name)) {
                        Ok(()) => self.reload_config().await,
                        Err(e) => println!("{} 删除失败：{e:#}", red("✗")),
                    }
                }
                "s" => {
                    let Some(value) = prompt("搜索范围（服务器名称，留空=全部服务器）：")? else {
                        continue;
                    };
                    if value.is_empty() {
                        self.scope = None;
                        println!("{} 搜索范围已设为全部服务器", green("✓"));
                    } else if self.cfg.servers.contains_key(&value) {
                        self.scope = Some(value.clone());
                        println!("{} 搜索范围已设为 {value}", green("✓"));
                    } else {
                        println!("{} 没有名为 {value} 的服务器", yellow("!"));
                    }
                }
                "0" | "q" | "" => return Ok(()),
                _ => println!("{} 无效选择", yellow("!")),
            }
        }
    }

    async fn login_flow(&mut self) -> Result<bool> {
        print_section("添加/登录服务器");
        let Some(server) = ask("服务器地址（如 https://media.example.com，回车取消）：")?
        else {
            return Ok(false);
        };
        let name = prompt("服务器别名（可留空）：")?.unwrap_or_default();
        print_options(&[("1)", "API Key"), ("2)", "账号密码")]);
        let mode = prompt("登录方式 [1/2，默认 2]：")?.unwrap_or_default();
        let (user, password, api_key) = if mode.trim() == "1" {
            let Some(key) = ask("API Key（回车取消）：")? else {
                return Ok(false);
            };
            (None, None, Some(key))
        } else {
            let Some(user) = ask("用户名（回车取消）：")? else {
                return Ok(false);
            };
            let Some(password) = read_secret("密码（回车取消）：")? else {
                return Ok(false);
            };
            (Some(user), Some(password), None)
        };
        let insecure = confirm("跳过 TLS 证书校验（自签名服务器）？", false)?;
        let args = LoginArgs {
            server,
            name: (!name.trim().is_empty()).then_some(name),
            user,
            password,
            api_key,
            insecure,
        };
        match crate::login(args).await {
            Ok(()) => Ok(true),
            Err(e) => {
                println!("登录失败：{e:#}");
                Ok(false)
            }
        }
    }

    // ───────────────────────── 设置 ─────────────────────────

    async fn settings_menu(&mut self) -> Result<()> {
        loop {
            print_section("下载设置");
            let s = &self.cfg.settings;
            let temp = if s.temp_dir.trim().is_empty() {
                "./downloads"
            } else {
                s.temp_dir.trim()
            };
            let final_dir = if s.final_dir.trim().is_empty() {
                "（未设置）"
            } else {
                s.final_dir.trim()
            };
            let limit = if s.limit.trim().is_empty() {
                "不限".to_string()
            } else {
                s.limit.trim().to_string()
            };
            let pattern = if s.pattern.trim().is_empty() {
                "内置默认".to_string()
            } else {
                truncate_display(s.pattern.trim(), 48)
            };
            let rows = [
                ("1", "临时文件夹（下载目录）", temp.to_string()),
                ("2", "最终文件夹（同步目标）", final_dir.to_string()),
                ("3", "每文件连接数", s.connections.to_string()),
                ("4", "每服务器并发", s.jobs.to_string()),
                ("5", "限速", limit),
                ("6", "下载外挂字幕", on_off(s.subs).to_string()),
                ("7", "覆盖已存在文件", on_off(s.overwrite).to_string()),
                ("8", "断点续传", on_off(s.resume).to_string()),
                ("9", "命名模板", pattern),
            ];
            let mut table = Table::new(&["#", "设置项", "当前值"]).align(0, Align::Right);
            for (key, name, value) in rows {
                table.row(vec![key.to_string(), name.to_string(), value]);
            }
            println!("{}", table.render());
            println!("  {}", dim("修改后立即保存到配置文件。"));
            print_options(&[("0)", "返回")]);

            let Some(choice) = prompt("选择要修改的项：")? else {
                return Ok(());
            };
            match choice.as_str() {
                "1" => {
                    if let Some(value) = prompt("新的临时文件夹（下载目录；回车取消）：")?
                        && !value.trim().is_empty()
                    {
                        self.update_settings(|s| s.temp_dir = value.trim().to_string());
                    }
                }
                "2" => {
                    if let Some(value) = prompt("新的最终文件夹（同步目标；回车取消）：")?
                    {
                        self.update_settings(|s| s.final_dir = value.trim().to_string());
                    }
                }
                "3" => {
                    if let Some(value) = prompt("每文件连接数 1-32（回车取消）：")? {
                        match value.trim().parse::<usize>() {
                            Ok(n) => self.update_settings(|s| s.connections = n.clamp(1, 32)),
                            Err(_) => println!("请输入数字"),
                        }
                    }
                }
                "4" => {
                    if let Some(value) = prompt("每服务器并发数 1-8（回车取消）：")? {
                        match value.trim().parse::<usize>() {
                            Ok(n) => self.update_settings(|s| s.jobs = n.clamp(1, 8)),
                            Err(_) => println!("请输入数字"),
                        }
                    }
                }
                "5" => {
                    if let Some(value) =
                        prompt("新的限速（如 5M / 800K；留空=不限速；回车取消）：")?
                    {
                        let value = value.trim();
                        if value.is_empty() {
                            self.update_settings(|s| s.limit.clear());
                        } else if parse_rate(value).is_some() {
                            self.update_settings(|s| s.limit = value.to_string());
                        } else {
                            println!("无法解析 “{value}”，示例：5M、800K");
                        }
                    }
                }
                "6" => self.update_settings(|s| s.subs = !s.subs),
                "7" => self.update_settings(|s| s.overwrite = !s.overwrite),
                "8" => self.update_settings(|s| s.resume = !s.resume),
                "9" => {
                    if let Some(value) = prompt("新的命名模板（留空=内置默认；回车取消）：")?
                    {
                        self.update_settings(|s| s.pattern = value);
                    }
                }
                "0" | "q" | "" => return Ok(()),
                _ => println!("无效选择"),
            }
        }
    }

    fn update_settings(&mut self, update: impl FnOnce(&mut Settings)) {
        let mut settings = self.cfg.settings.clone();
        update(&mut settings);
        let to_write = settings.clone();
        match config::edit(move |cfg| {
            cfg.settings = to_write;
            Ok(())
        }) {
            Ok(()) => {
                self.cfg.settings = settings;
                println!("{} 已保存", green("✓"));
            }
            Err(e) => println!("{} 保存设置失败：{e:#}", red("✗")),
        }
    }

    // ───────────────────────── 未完成任务 ─────────────────────────

    async fn unfinished_menu(&mut self) -> Result<()> {
        print_section("未完成任务找回");
        if self.clients.is_empty() {
            println!(
                "{} 没有可用的服务器，请先在「服务器管理」中登录",
                yellow("!")
            );
            return Ok(());
        }
        let out = output_path(&self.cfg.settings)?;
        let clients: Vec<(String, Client)> = self
            .clients
            .iter()
            .map(|(name, client)| (name.clone(), client.clone()))
            .collect();
        println!("  正在扫描 {} 并搜索来源…", dim(&out.display().to_string()));
        let result = repair::scan_and_search(&out, &clients)
            .await
            .context("扫描未完成任务失败")?;
        if result.tasks.is_empty() {
            println!("  {}", dim("临时文件夹里没有未完成任务"));
            return Ok(());
        }
        if result.total > result.tasks.len() {
            println!(
                "  {}",
                dim(&format!(
                    "共 {} 个未完成任务，先显示前 {} 个",
                    result.total,
                    result.tasks.len()
                ))
            );
        }

        let mut flat: Vec<(usize, usize)> = Vec::new();
        for (ti, task) in result.tasks.iter().enumerate() {
            println!();
            println!(
                "  {} {}",
                dim(&format!("{}.", ti + 1)),
                bold(&task.file.dest.display().to_string())
            );
            println!(
                "     {} · 关键字 “{}”",
                dim(&format!("已有 {}", format_bytes(task.file.partial_bytes))),
                yellow(&task.file.keyword)
            );
            for error in &task.errors {
                println!("     {} {error}", yellow("!"));
            }
            if task.candidates.is_empty() {
                println!("     {}", dim("（没有找到来源）"));
                continue;
            }
            let mut table =
                Table::new(&["#", "服务器", "类型", "大小", "名称"]).align(0, Align::Right);
            for (ci, candidate) in task.candidates.iter().enumerate() {
                flat.push((ti, ci));
                table.row(vec![
                    flat.len().to_string(),
                    candidate.server.clone(),
                    candidate.item.display_type().to_string(),
                    candidate
                        .item
                        .size()
                        .map(format_bytes)
                        .unwrap_or_else(|| "-".into()),
                    candidate.item.title(),
                ]);
            }
            println!("{}", table.render());
        }

        let Some(idx) = choose(flat.len(), "选择来源编号（回车取消）：")? else {
            return Ok(());
        };
        let (ti, ci) = flat[idx];
        let task = &result.tasks[ti];
        let candidate = &task.candidates[ci];
        let Some(client) = self.clients.get(&candidate.server).cloned() else {
            println!("{} 服务器 {} 未连接", yellow("!"), candidate.server);
            return Ok(());
        };
        if !confirm(
            &format!("写回 {} 并断点续传？", task.file.dest.display()),
            true,
        )? {
            return Ok(());
        }
        let mut opts = build_opts(&self.cfg.settings)?;
        opts.fixed_dest = Some(task.file.dest.clone());
        self.daemon.enqueue(
            candidate.server.clone(),
            client,
            vec![candidate.item.clone()],
            opts,
        );
        println!(
            "{} 已加入 {} 的下载队列，将写回 {}",
            green("✓"),
            candidate.server,
            task.file.dest.display()
        );
        Ok(())
    }

    // ───────────────────────── 历史 ─────────────────────────

    async fn history_menu(&self) -> Result<()> {
        print_section("下载记录");
        let Some(value) = prompt("显示条数（回车=50，0=全部）：")? else {
            return Ok(());
        };
        let limit = if value.is_empty() {
            50
        } else {
            value.parse::<usize>().unwrap_or(50)
        };
        crate::print_history(limit)
    }

    fn sync_menu(&self) -> Result<()> {
        print_section("同步到最终文件夹");
        let temp = self.cfg.settings.temp_dir.trim();
        let final_dir = self.cfg.settings.final_dir.trim();
        print_key_value(
            "临时文件夹",
            if temp.is_empty() { "./downloads" } else { temp },
        );
        print_key_value(
            "最终文件夹",
            if final_dir.is_empty() {
                "（未设置）"
            } else {
                final_dir
            },
        );
        if final_dir.is_empty() {
            println!(
                "{} 请先在「下载设置」中设置最终文件夹，或使用 embydown sync --final <DIR>",
                yellow("!")
            );
            return Ok(());
        }

        // 先展示预览，确认后再实际复制。
        crate::review::sync(None, None, true, false)?;
        if !confirm("开始同步？", true)? {
            println!("{} 已取消", yellow("!"));
            return Ok(());
        }
        let force = confirm("大小相同的文件也重新复制？", false)?;
        crate::review::sync(None, None, false, force)
    }

    fn choose_server(&self, hint: &str) -> Result<Option<String>> {
        let names: Vec<String> = self.clients.keys().cloned().collect();
        choose_name(&names, hint)
    }
}

// ───────────────────────── 搜索结果聚合 ─────────────────────────

struct Hit {
    server: String,
    item: Item,
}

/// 跨服务器聚合后的“一部影片/一集”。
struct TitleGroup {
    title: String,
    kind: String,
    year: Option<i32>,
    season: Option<i32>,
    episode: Option<i32>,
    score: i64,
    hits: Vec<Hit>,
}

struct SourceOption {
    server: String,
    item: Item,
    kind: String,
    year: String,
    version: String,
    size: Option<u64>,
    spec: String,
    name: String,
}

fn group_hits(hits: Vec<Hit>, term: &str) -> Vec<TitleGroup> {
    let mut groups: Vec<TitleGroup> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for hit in hits {
        let key = hit.item.search_group_key();
        let score = hit.item.search_score(term);
        if let Some(&i) = index.get(&key) {
            let group = &mut groups[i];
            group.score = group.score.max(score);
            group.hits.push(hit);
            continue;
        }
        let is_episode = hit.item.item_type.as_deref() == Some("Episode");
        index.insert(key, groups.len());
        groups.push(TitleGroup {
            title: group_title(&hit.item),
            kind: hit.item.display_type().to_string(),
            year: hit.item.production_year.filter(|y| *y >= 1800),
            season: is_episode.then(|| hit.item.season_no()),
            episode: is_episode.then(|| hit.item.episode_no()),
            score,
            hits: vec![hit],
        });
    }
    groups.sort_by_cached_key(|g| {
        (
            std::cmp::Reverse(g.score),
            g.season.unwrap_or(0),
            g.episode.unwrap_or(0),
            g.title.chars().count(),
            g.title.to_lowercase(),
        )
    });
    groups
}

fn group_title(item: &Item) -> String {
    if item.item_type.as_deref() == Some("Episode") {
        let series = item.series_name.as_deref().unwrap_or("").trim();
        let season = item.season_no();
        let episode = item.episode_no();
        if !series.is_empty() && episode > 0 {
            let title = item.title();
            if title.is_empty() || title == series {
                return format!("{series} S{season:02}E{episode:02}");
            }
            return format!("{series} S{season:02}E{episode:02} · {title}");
        }
    }
    item.title()
}

fn source_options(group: &TitleGroup) -> Vec<SourceOption> {
    let mut out = Vec::new();
    for hit in &group.hits {
        let sources = hit.item.media_sources.as_deref().unwrap_or_default();
        let (size, res, video, audio, container) = if let Some(src) = sources.first() {
            crate::file_info_source(&hit.item, Some(src))
        } else {
            crate::file_info(&hit.item)
        };
        let version = match sources.len() {
            0 => hit.item.version.clone().unwrap_or_default(),
            1 => crate::source_version_label(&hit.item, &sources[0], 0),
            n => {
                let labels: Vec<String> = sources
                    .iter()
                    .enumerate()
                    .map(|(i, src)| crate::source_version_label(&hit.item, src, i))
                    .filter(|label| !label.is_empty())
                    .collect();
                match labels.first() {
                    Some(first) => format!("{first} 等 {n} 个版本"),
                    None => format!("{n} 个版本"),
                }
            }
        };
        out.push(SourceOption {
            server: hit.server.clone(),
            item: hit.item.clone(),
            kind: hit.item.display_type().to_string(),
            year: hit
                .item
                .production_year
                .filter(|y| *y >= 1800)
                .map(|y| y.to_string())
                .unwrap_or_else(|| "-".into()),
            version,
            size,
            spec: spec_label(&res, &video, &audio, &container),
            name: hit.item.title(),
        });
    }
    out
}

fn print_groups(groups: &[TitleGroup]) {
    let mut table =
        Table::new(&["#", "类型", "年份", "季/集", "来源", "名称"]).align(0, Align::Right);
    for (i, group) in groups.iter().enumerate() {
        let servers: BTreeSet<&str> = group.hits.iter().map(|h| h.server.as_str()).collect();
        let episode = group
            .episode
            .filter(|e| *e > 0)
            .map(|e| format!("S{:02}E{:02}", group.season.unwrap_or(0), e))
            .unwrap_or_else(|| "-".into());
        table.row(vec![
            (i + 1).to_string(),
            group.kind.clone(),
            group
                .year
                .map(|y| y.to_string())
                .unwrap_or_else(|| "-".into()),
            episode,
            format!("{} 服 / {} 源", servers.len(), group.hits.len()),
            group.title.clone(),
        ]);
    }
    println!("{}", table.render());
}

fn print_sources(options: &[SourceOption]) {
    let mut table = Table::new(&[
        "#",
        "服务器",
        "类型",
        "年份",
        "版本",
        "大小",
        "规格",
        "名称",
    ])
    .align(0, Align::Right)
    .align(5, Align::Right);
    for (i, option) in options.iter().enumerate() {
        table.row(vec![
            (i + 1).to_string(),
            option.server.clone(),
            option.kind.clone(),
            option.year.clone(),
            option.version.clone(),
            option.size.map(format_bytes).unwrap_or_else(|| "-".into()),
            option.spec.clone(),
            option.name.clone(),
        ]);
    }
    println!("{}", table.render());
}

// ───────────────────────── 版本 / 季 ─────────────────────────

fn choose_seasons(mut files: Vec<Item>) -> Result<Option<Vec<Item>>> {
    let mut seasons: Vec<i32> = files
        .iter()
        .filter(|item| item.item_type.as_deref() == Some("Episode"))
        .map(Item::season_no)
        .collect();
    seasons.sort_unstable();
    seasons.dedup();
    if seasons.len() <= 1 {
        return Ok(Some(files));
    }
    let list = seasons
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    println!("包含的季：{list}");
    loop {
        let Some(value) = prompt("选择要下载的季（如 1,2；回车=全部，输入 ! 取消）：")?
        else {
            return Ok(None);
        };
        let value = value.trim();
        if value == "!" {
            return Ok(None);
        }
        if value.is_empty() {
            return Ok(Some(files));
        }
        let mut wanted = Vec::new();
        let mut invalid = false;
        for part in value.split([',', ' ', '，']) {
            if part.is_empty() {
                continue;
            }
            match part.parse::<i32>() {
                Ok(n) => wanted.push(n),
                Err(_) => {
                    invalid = true;
                    break;
                }
            }
        }
        if invalid || wanted.is_empty() {
            println!("格式无效，示例：1,2");
            continue;
        }
        files.retain(|item| keep_season(item, &wanted));
        return Ok(Some(files));
    }
}

fn keep_season(item: &Item, wanted: &[i32]) -> bool {
    if item.item_type.as_deref() == Some("Episode") {
        wanted.contains(&item.season_no())
    } else {
        true
    }
}

fn choose_versions(files: Vec<Item>) -> Result<Option<Vec<Item>>> {
    if !has_multiple_versions(&files) {
        return Ok(Some(files));
    }
    let versions = version_summaries(&files);
    if versions.is_empty() {
        println!("检测到多个版本但无法识别版本名，将下载全部版本");
        return Ok(Some(expand_versions(files)));
    }

    let mut table = Table::new(&["#", "版本", "数量", "大小", "分辨率", "容器"])
        .align(0, Align::Right)
        .align(3, Align::Right);
    for (i, version) in versions.iter().enumerate() {
        let count = if version.is_episode {
            format!("{} 集", version.count)
        } else {
            format!("{} 个", version.count)
        };
        table.row(vec![
            (i + 1).to_string(),
            version.label.clone(),
            count,
            version.size.map(format_bytes).unwrap_or_else(|| "-".into()),
            version.res.clone(),
            version.container.clone(),
        ]);
    }
    println!("检测到 {} 个版本：", versions.len());
    println!("{}", table.render());

    loop {
        let Some(value) = prompt("选择版本 [1-N / a=全部]（回车取消）：")? else {
            return Ok(None);
        };
        let value = value.trim();
        if value.is_empty() {
            return Ok(None);
        }
        if value.eq_ignore_ascii_case("a") || value.eq_ignore_ascii_case("all") {
            return Ok(Some(expand_versions(files)));
        }
        match value.parse::<usize>() {
            Ok(n) if n >= 1 && n <= versions.len() => {
                let label = versions[n - 1].label.clone();
                let (kept, skipped) = filter_to_version(files, &label);
                if skipped > 0 {
                    println!("ℹ️  版本 “{label}”：跳过 {skipped} 个不含该版本的条目");
                }
                return Ok(Some(kept));
            }
            _ => println!("请输入 1-{} 或 a", versions.len()),
        }
    }
}

// ───────────────────────── 终端样式 ─────────────────────────

fn color_enabled() -> bool {
    io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM")
            .map(|term| term != "dumb")
            .unwrap_or(true)
}

fn paint(text: &str, code: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn bold(text: &str) -> String {
    paint(text, "1")
}

fn cyan(text: &str) -> String {
    paint(text, "1;36")
}

fn dim(text: &str) -> String {
    paint(text, "2")
}

fn green(text: &str) -> String {
    paint(text, "32")
}

fn yellow(text: &str) -> String {
    paint(text, "33")
}

fn red(text: &str) -> String {
    paint(text, "31")
}

/// 显示宽度补空格（中文占 2 列）。
fn pad_display(text: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(text);
    format!("{text}{}", " ".repeat(width.saturating_sub(used)))
}

fn truncate_display(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width.saturating_sub(1) {
            out.push('…');
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

fn print_rule(width: usize) {
    println!("{}", dim(&"─".repeat(width)));
}

fn print_section(title: &str) {
    println!();
    println!("{} {}", cyan("▸"), bold(title));
    print_rule(60);
}

fn print_key_value(label: &str, value: &str) {
    println!("  {} {}", dim(&pad_display(label, 10)), value);
}

/// 选项列表：每行只放一个选项。
fn print_options(items: &[(&str, &str)]) {
    for (key, label) in items {
        println!("  {} {}", cyan(key), label);
    }
}

// ───────────────────────── 工具函数 ─────────────────────────

/// 菜单里的临时错误不应该让整个交互模式退出。
async fn run_step<F>(future: F)
where
    F: std::future::Future<Output = Result<()>>,
{
    if let Err(e) = future.await {
        println!("❌ {e:#}");
    }
}

fn build_opts(settings: &Settings) -> Result<DownloadOpts> {
    let limit = settings.limit.trim();
    let limit_rate = if limit.is_empty() {
        None
    } else {
        Some(parse_rate(limit).with_context(|| format!("无法解析限速值：{limit}"))?)
    };
    Ok(DownloadOpts {
        out: output_path(settings)?,
        pattern: Some(settings.pattern.clone()).filter(|p| !p.trim().is_empty()),
        connections: settings.connections.clamp(1, 32),
        jobs: settings.jobs.clamp(1, 8),
        limit_rate,
        resume: settings.resume,
        overwrite: settings.overwrite,
        subs: settings.subs,
        dry_run: false,
        fixed_dest: None,
    })
}

fn output_path(settings: &Settings) -> Result<PathBuf> {
    let out = settings.temp_dir.trim();
    let out = if out.is_empty() { "./downloads" } else { out };
    crate::util::expand_tilde(Path::new(out))
}

fn choose_name(names: &[String], hint: &str) -> Result<Option<String>> {
    if names.is_empty() {
        println!("{} 没有可用的服务器", yellow("!"));
        return Ok(None);
    }
    if names.len() == 1 {
        println!("{} 使用 {}", green("✓"), names[0]);
        return Ok(Some(names[0].clone()));
    }
    for (i, name) in names.iter().enumerate() {
        println!("  {} {}", cyan(&format!("{})", i + 1)), name);
    }
    let Some(idx) = choose(names.len(), hint)? else {
        return Ok(None);
    };
    Ok(Some(names[idx].clone()))
}

fn choose(len: usize, hint: &str) -> Result<Option<usize>> {
    if len == 0 {
        return Ok(None);
    }
    loop {
        let Some(value) = prompt(hint)? else {
            return Ok(None);
        };
        if value.is_empty() {
            return Ok(None);
        }
        match value.parse::<usize>() {
            Ok(n) if n >= 1 && n <= len => return Ok(Some(n - 1)),
            _ => println!("{} 请输入 1-{len} 的数字", yellow("!")),
        }
    }
}

fn prompt(hint: &str) -> Result<Option<String>> {
    print!("{}", dim(hint));
    io::stdout().flush().context("刷新输出失败")?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line).context("读取输入失败")? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim().to_string()))
}

/// 必填输入：直接回车视为取消。
fn ask(hint: &str) -> Result<Option<String>> {
    let Some(value) = prompt(hint)? else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    Ok(Some(value))
}

fn confirm(hint: &str, default_yes: bool) -> Result<bool> {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    let Some(value) = prompt(&format!("{hint} {suffix}: "))? else {
        return Ok(false);
    };
    if value.is_empty() {
        return Ok(default_yes);
    }
    Ok(matches!(value.to_lowercase().as_str(), "y" | "yes" | "是"))
}

fn read_secret(hint: &str) -> Result<Option<String>> {
    print!("{}", dim(hint));
    io::stdout().flush().context("刷新输出失败")?;
    let mut guard = EchoGuard::default();
    if io::stdin().is_terminal() {
        guard.0 = std::process::Command::new("stty")
            .arg("-echo")
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    }
    let mut line = String::new();
    let read = io::stdin().read_line(&mut line);
    drop(guard);
    println!();
    if read.context("读取密码失败")? == 0 {
        return Ok(None);
    }
    let value = line.trim_end_matches(['\r', '\n']).to_string();
    if value.is_empty() {
        return Ok(None);
    }
    Ok(Some(value))
}

#[derive(Default)]
struct EchoGuard(bool);

impl Drop for EchoGuard {
    fn drop(&mut self) {
        if self.0 {
            let _ = std::process::Command::new("stty").arg("echo").status();
        }
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "开" } else { "关" }
}

fn type_label(item: &Item) -> &'static str {
    match item.item_type.as_deref() {
        Some("Series") => "剧",
        Some("Season") => "季",
        Some("Episode") => "集",
        Some("Movie") => "影",
        Some("Audio") => "音",
        Some("MusicAlbum") | Some("MusicArtist") => "乐",
        Some("CollectionFolder") | Some("Folder") | Some("BoxSet") => "夹",
        _ => "·",
    }
}

fn item_meta(item: &Item) -> String {
    let mut parts = Vec::new();
    if let Some(year) = item.production_year.filter(|y| *y >= 1800) {
        parts.push(year.to_string());
    }
    if item.item_type.as_deref() == Some("Episode") {
        parts.push(format!("S{:02}E{:02}", item.season_no(), item.episode_no()));
    }
    if let Some(size) = item.size() {
        parts.push(format_bytes(size));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("  {}", parts.join(" · "))
    }
}

fn spec_label(res: &str, video: &str, audio: &str, container: &str) -> String {
    let mut parts = Vec::new();
    for part in [res, video, audio, container] {
        if !part.is_empty() {
            parts.push(part.to_string());
        }
    }
    if parts.is_empty() {
        "-".into()
    } else {
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_movie_across_servers() {
        let movie = |name: &str| Item {
            name: Some(name.into()),
            item_type: Some("Movie".into()),
            production_year: Some(2021),
            ..Default::default()
        };
        let hits = vec![
            Hit {
                server: "a".into(),
                item: movie("Spider-Man: No Way Home"),
            },
            Hit {
                server: "b".into(),
                item: movie("spider man no way home"),
            },
        ];
        let groups = group_hits(hits, "spider man");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].hits.len(), 2);
    }

    #[test]
    fn groups_episode_by_series_season_and_episode() {
        let episode = |title: &str, episode: i32| Item {
            name: Some(title.into()),
            item_type: Some("Episode".into()),
            series_name: Some("庆余年".into()),
            parent_index_number: Some(1),
            index_number: Some(episode),
            ..Default::default()
        };
        let hits = vec![
            Hit {
                server: "a".into(),
                item: episode("初入京都", 1),
            },
            Hit {
                server: "b".into(),
                item: episode("第一集", 1),
            },
            Hit {
                server: "a".into(),
                item: episode("第二集", 2),
            },
        ];
        let groups = group_hits(hits, "庆余年");
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].title, "庆余年 S01E01 · 初入京都");
        assert_eq!(groups[0].hits.len(), 2);
    }

    #[test]
    fn source_list_keeps_a_single_source() {
        let group = TitleGroup {
            title: "阿凡达".into(),
            kind: "Movie".into(),
            year: Some(2009),
            season: None,
            episode: None,
            score: 5000,
            hits: vec![Hit {
                server: "media".into(),
                item: Item {
                    name: Some("阿凡达".into()),
                    item_type: Some("Movie".into()),
                    production_year: Some(2009),
                    media_sources: Some(vec![crate::model::MediaSource {
                        name: Some("阿凡达 (2009) 1080p.mkv".into()),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
            }],
        };
        let options = source_options(&group);
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].server, "media");
        assert_eq!(options[0].version, "1080p");
    }

    #[test]
    fn seasons_are_filtered_by_number() {
        let episode = |season: i32| Item {
            name: Some(format!("S{season}")),
            item_type: Some("Episode".into()),
            parent_index_number: Some(season),
            index_number: Some(1),
            ..Default::default()
        };
        let files = vec![episode(1), episode(2), episode(1)];
        let kept: Vec<Item> = files
            .into_iter()
            .filter(|item| keep_season(item, &[1]))
            .collect();
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|item| item.season_no() == 1));
    }
}
