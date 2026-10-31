//! embydown —— Emby 媒体下载工具（默认直链）。

mod cli;
mod cd2;
mod config;
mod daemon;
mod download;
mod emby;
mod instant;
mod interactive;
mod manager;
mod model;
mod naming;
mod repair;
mod review;
mod store;
mod table;
mod update;
mod util;

use anyhow::{bail, Context, Result};
use clap::Parser;
use futures_util::future::join_all;
use futures_util::StreamExt;
use std::collections::HashSet;
use std::io::Write;

use crate::cli::{Cli, Cmd, LoginArgs};
use crate::config::{Config, Profile};
use crate::download::DownloadOpts;
use crate::emby::Client;
use crate::model::{Item, MediaSource};
use crate::table::{Align, Table};
use crate::util::{format_bytes, format_ticks};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    util::set_verbose(cli.verbose);
    if let Err(err) = run(cli).await {
        eprintln!("\n❌ {}", crate::emby::redact_text(&format!("{err:#}")));
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let Some(cmd) = cli.cmd else {
        return interactive::run().await;
    };
    match cmd {
        Cmd::Ui => interactive::run().await,
        Cmd::Daemon => daemon::run().await,
        Cmd::History { limit } => print_history(limit),
        Cmd::Pause { server } => {
            daemon::cli_pause(server.as_deref()).await?;
            match &server {
                Some(name) => println!("已暂停 {name} 的下载队列"),
                None => println!("已暂停所有服务器的下载队列"),
            }
            Ok(())
        }
        Cmd::Resume { server } => {
            daemon::cli_resume(server.as_deref()).await?;
            match &server {
                Some(name) => println!("已继续 {name} 的下载队列"),
                None => println!("已继续所有服务器的下载队列"),
            }
            Ok(())
        }
        Cmd::Login(args) => login(args).await,
        Cmd::Logout { name } => logout(name),
        Cmd::Servers => list_servers(),
        Cmd::Update {
            check,
            version,
            force,
        } => update::run(check, version, force).await,
        Cmd::Review { temp, final_dir } => review::run(temp, final_dir),
        Cmd::Sync {
            dry_run,
            force,
            temp,
            final_dir,
        } => review::sync(temp, final_dir, dry_run, force),
        Cmd::UploadTest {
            server,
            target,
            limit,
            cd2_url,
            cd2_token,
            window,
            keep,
            temp,
            final_dir,
        } => {
            instant::run(
                &target,
                server.server.as_deref(),
                limit,
                instant::Options {
                    cd2_url,
                    cd2_token,
                    window: std::time::Duration::from_secs(window.max(1)),
                    keep,
                    temp_dir: temp,
                    final_dir,
                },
            )
            .await
        }
        cmd => run_command(cmd).await,
    }
}

/// 登录：验证凭证并按别名保存，可重复执行以添加多个服务器。
async fn login(args: LoginArgs) -> Result<()> {
    let mut client = Client::new(&args.server, args.insecure)?;

    let username = match (args.api_key, args.user, args.password) {
        (Some(key), _, _) if !key.trim().is_empty() => {
            client.token = key.trim().to_string();
            client.resolve_user().await?;
            None
        }
        (_, Some(u), Some(p)) => {
            client.login(&u, &p).await?;
            client.resolve_user().await?;
            Some(u)
        }
        (_, Some(_), None) => bail!("缺少 --password"),
        (_, None, Some(_)) => bail!("缺少 --user"),
        _ => bail!("需要 --api-key，或 --user 与 --password"),
    };

    let name = args
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| config::name_from_server(&client.server));

    let profile = Profile {
        server: client.server.clone(),
        token: client.token.clone(),
        user_id: client.user_id.clone(),
        username: username.clone(),
        insecure: args.insecure,
    };
    let (replaced, count) = config::edit(|cfg| {
        let replaced = cfg.servers.insert(name.clone(), profile);
        Ok((replaced, cfg.servers.len()))
    })?;
    let path = config::path()?;

    if let Some(old) = replaced
        && old.server != client.server
    {
        println!("⚠️  已覆盖同名服务器 {name}（原地址 {}）", old.server);
    }
    match username {
        Some(u) => println!("✅ 已登录 {}（名称 {name}，用户 {u}）", client.server),
        None => println!("✅ 已登录 {}（名称 {name}，API Key）", client.server),
    }
    println!(
        "📝 登录信息已保存到 {}（共 {} 个服务器）",
        path.display(),
        count
    );
    Ok(())
}

fn logout(name: Option<String>) -> Result<()> {
    match name {
        Some(name) => {
            let remaining = config::edit(|cfg| {
                if cfg.servers.remove(&name).is_none() {
                    bail!(
                        "没有名为 {name} 的服务器；已登录：{}（用 embydown servers 查看）",
                        cfg.name_list()
                    );
                }
                Ok(cfg.servers.len())
            })?;
            if remaining == 0 {
                // 保留 settings：退出登录不应该丢掉下载目录等设置。
                println!("👋 已退出 {name}（已无登录服务器）");
            } else {
                println!("👋 已退出 {name}（剩余 {remaining} 个服务器）");
            }
        }
        None => {
            let has_servers = config::load_optional()?
                .map(|cfg| !cfg.servers.is_empty())
                .unwrap_or(false);
            if has_servers {
                config::edit(|cfg| {
                    cfg.servers.clear();
                    Ok(())
                })?;
                println!("👋 已清除所有登录信息（下载设置已保留）");
            } else {
                println!("当前没有保存的登录信息");
            }
        }
    }
    Ok(())
}

/// `embydown history`：读取守护进程永久保存的下载记录（最新在前）。
fn print_history(limit: usize) -> Result<()> {
    let records = store::load_history();
    if records.is_empty() {
        println!("暂无下载记录");
        return Ok(());
    }
    let shown = if limit == 0 {
        records.len()
    } else {
        limit.min(records.len())
    };
    println!(
        "{:<19}  {:<16} {:<6} {:>10}  NAME",
        "TIME (UTC)", "SERVER", "STATUS", "SIZE"
    );
    println!("{}", "-".repeat(96));
    for rec in records.iter().rev().take(shown) {
        let f = &rec.entry;
        let status = match f.status {
            manager::FinishedStatus::Done => "完成",
            manager::FinishedStatus::Skipped => "跳过",
            manager::FinishedStatus::Failed => "失败",
            manager::FinishedStatus::Cancelled => "取消",
        };
        println!(
            "{:<19}  {} {} {:>10}  {}",
            util::format_unix_time(rec.time_unix),
            pad_display(&truncate_display(&f.server, 16), 16),
            pad_display(status, 6),
            format_bytes(f.bytes),
            f.title
        );
        if f.status == manager::FinishedStatus::Failed
            && let Some(err) = &f.error
        {
            let brief = err.lines().next().unwrap_or(err);
            println!("    ↳ {}", truncate_display(brief, 88));
        }
    }
    println!(
        "\n共 {} 条记录（显示 {} 条）；记录保存在 {}",
        records.len(),
        shown,
        store::history_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "history.jsonl".into())
    );
    Ok(())
}

/// 按显示宽度截断（用于表格对齐；中文按 2 列计）。
fn truncate_display(s: &str, width: usize) -> String {
    if unicode_width::UnicodeWidthStr::width(s) <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width.saturating_sub(1) {
            out.push('…');
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

/// 按显示宽度补空格（中文按 2 列计），用于终端表格对齐。
fn pad_display(s: &str, width: usize) -> String {
    let used = unicode_width::UnicodeWidthStr::width(s);
    if used >= width {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + width - used);
    out.push_str(s);
    out.extend(std::iter::repeat_n(' ', width - used));
    out
}

fn list_servers() -> Result<()> {
    let cfg = match config::load_optional()? {
        Some(cfg) if !cfg.is_empty() => cfg,
        _ => {
            println!("尚未登录任何服务器");
            return Ok(());
        }
    };
    println!("{:<24} {:<40} USER", "NAME", "SERVER");
    println!("{}", "-".repeat(86));
    for (name, p) in &cfg.servers {
        println!(
            "{:<24} {:<40} {}",
            name,
            p.server,
            p.username.as_deref().unwrap_or("(API Key)")
        );
    }
    println!("\n共 {} 个服务器", cfg.servers.len());
    Ok(())
}

/// 用保存的 Token 构造客户端；必要时补一次 /Users/Me。
async fn client_for(name: &str, profile: &Profile) -> Result<Client> {
    let mut client = Client::new(&profile.server, profile.insecure)?;
    client.token = profile.token.clone();
    if profile.user_id.is_empty() {
        client
            .resolve_user()
            .await
            .with_context(|| format!("恢复 {name} 的登录状态失败"))?;
    } else {
        client.user_id = profile.user_id.clone();
    }
    util::vlog(
        1,
        format!(
            "🔑 已连接 {name}（{}，用户 {}）",
            client.server,
            profile.username.as_deref().unwrap_or("-")
        ),
    );
    Ok(client)
}

/// 没有 --server 时：单服务器自动使用，多服务器要求指定。
fn pick_one<'a>(cfg: &'a Config, name: Option<&str>) -> Result<(&'a str, &'a Profile)> {
    match name {
        Some(n) => cfg
            .servers
            .get_key_value(n)
            .map(|(k, v)| (k.as_str(), v))
            .with_context(|| {
                format!(
                    "没有名为 {n} 的服务器；已登录：{}（用 embydown servers 查看）",
                    cfg.name_list()
                )
            }),
        None => cfg.only(),
    }
}

async fn run_command(cmd: Cmd) -> Result<()> {
    // 直链下载不需要登录，所以这里允许配置为空；需要认证的分支会自行报错。
    let cfg = config::load_optional()?.unwrap_or_default();

    match cmd {
        Cmd::List {
            server,
            parent,
            recursive,
            kinds,
            limit,
        } => {
            let (name, profile) = pick_one(&cfg, server.server.as_deref())?;
            let client = client_for(name, profile).await?;
            let items = match parent {
                None => client.views().await?,
                Some(p) => {
                    let types = if kinds.is_empty() {
                        None
                    } else {
                        Some(kinds.join(","))
                    };
                    client
                        .children(&p, recursive, types.as_deref(), Some(limit))
                        .await?
                }
            };
            if items.is_empty() {
                println!("（没有条目）");
            } else {
                print_table(&items);
            }
        }

        Cmd::Info { server, id } => {
            let (name, profile) = pick_one(&cfg, server.server.as_deref())?;
            let client = client_for(name, profile).await?;
            let item = client.item(&id).await?;
            print_detail(&item);
        }

        Cmd::Url { server, id } => {
            let (name, profile) = pick_one(&cfg, server.server.as_deref())?;
            let client = client_for(name, profile).await?;
            let item = client.item(&id).await?;
            println!("条目: {} ({}，服务器 {name})", item.title(), item.display_type());
            for (i, u) in client.media_urls(&item).iter().enumerate() {
                println!("{}[{}] {}", if i == 0 { "" } else { "备用" }, i, u);
            }
        }

        Cmd::Download {
            server,
            target,
            out,
            pattern,
            connections,
            jobs,
            limit_rate,
            no_limit_rate,
            limit,
            version,
            season,
            overwrite,
            no_overwrite,
            subs: subs_flag,
            no_subs,
            resume: resume_flag,
            no_resume,
            dry_run,
        } => {
            let is_url = target.starts_with("http://") || target.starts_with("https://");
            // --dry-run：列出匹配结果与文件信息（相当于搜索），不下载
            if dry_run && !is_url {
                preview_matches(&cfg, server.server.as_deref(), &target, limit).await?;
                return Ok(());
            }

            // 命令行显式参数优先；未指定时使用交互模式「下载设置」保存的配置。
            let settings = &cfg.settings;

            let out = match out {
                Some(p) if !p.as_os_str().is_empty() => p,
                _ => {
                    let configured = settings.temp_dir.trim();
                    if configured.is_empty() {
                        std::path::PathBuf::from("./downloads")
                    } else {
                        std::path::PathBuf::from(configured)
                    }
                }
            };
            let out = util::expand_tilde(&out)?;

            let connections = connections.unwrap_or(settings.connections).clamp(1, 32);
            let jobs = jobs.unwrap_or(settings.jobs).clamp(1, 8);

            let rate = if no_limit_rate {
                None
            } else if let Some(s) = limit_rate.as_deref() {
                let s = s.trim();
                if s.is_empty() {
                    None
                } else {
                    Some(util::parse_rate(s).with_context(|| format!("无法解析限速值: {s}"))?)
                }
            } else {
                let s = settings.limit.trim();
                if s.is_empty() {
                    None
                } else {
                    Some(util::parse_rate(s).with_context(|| {
                        format!(
                            "配置文件中的限速值无效: {s}（修改交互模式的「下载设置」或加 --no-limit-rate）"
                        )
                    })?)
                }
            };

            let pattern = match pattern {
                // 显式传空字符串表示“本次使用内置默认模板”，而不是回退到配置。
                Some(p) if p.trim().is_empty() => None,
                Some(p) => Some(p),
                None => {
                    let configured = settings.pattern.trim();
                    if configured.is_empty() {
                        None
                    } else {
                        Some(configured.to_string())
                    }
                }
            };

            let subs = if subs_flag {
                true
            } else if no_subs {
                false
            } else {
                settings.subs
            };
            let overwrite = if overwrite {
                true
            } else if no_overwrite {
                false
            } else {
                settings.overwrite
            };
            let resume = if resume_flag {
                true
            } else if no_resume {
                false
            } else {
                settings.resume
            };

            let (_server_name, client, root) = if is_url {
                (
                    "（直链）".to_string(),
                    Client::new("http://127.0.0.1", false)?,
                    direct_item(&target),
                )
            } else {
                pick_item(&cfg, server.server.as_deref(), &target, limit).await?
            };
            let mut items = resolve_item(&client, root, &season).await?;
            if items.is_empty() {
                bail!("没有匹配到可下载的条目（检查 ID / --season 过滤条件）");
            }
            apply_version_choice(&mut items, version.as_deref())?;
            util::vlog(1, format!("🎬 待下载 {} 个条目", items.len()));
            let opts = DownloadOpts {
                out,
                pattern,
                connections,
                // dry-run 下强制串行，避免多任务输出交错
                jobs: if dry_run { 1 } else { jobs },
                limit_rate: rate,
                resume,
                overwrite,
                subs,
                dry_run,
                fixed_dest: None,
            };
            download::execute(&client, items, opts).await?;
        }

        Cmd::Login(_)
        | Cmd::Logout { .. }
        | Cmd::Servers
        | Cmd::Ui
        | Cmd::Daemon
        | Cmd::Update { .. }
        | Cmd::Review { .. }
        | Cmd::Sync { .. }
        | Cmd::UploadTest { .. }
        | Cmd::History { .. }
        | Cmd::Pause { .. }
        | Cmd::Resume { .. } => {
            unreachable!(
                "login/logout/servers/ui/daemon/update/review/sync/upload-test/history/pause/resume 已在 run() 中处理"
            )
        }
    }

    Ok(())
}

/// `dl --dry-run`：列出匹配结果与文件信息（大小/分辨率/编码），相当于搜索。
async fn preview_matches(
    cfg: &Config,
    server: Option<&str>,
    target: &str,
    limit: u32,
) -> Result<()> {
    let targets = cfg.select(server)?;
    let mut clients = Vec::new();
    for (name, profile) in &targets {
        match client_for(name, profile).await {
            Ok(c) => clients.push((name.clone(), c)),
            Err(e) => eprintln!("⚠️  {name} 连接失败: {e:#}"),
        }
    }
    if clients.is_empty() {
        bail!("所有服务器都连接失败");
    }

    // 1) 明确写了 `名称:ID`
    if server.is_none()
        && let Some((prefix, id)) = target.split_once(':')
        && let Some(profile) = cfg.get(prefix)
    {
        let client = client_for(prefix, profile).await?;
        let item = client
            .item(id)
            .await
            .with_context(|| format!("在 {prefix} 上获取条目 {id} 失败"))?;
        let rows = search_rows(prefix, &client, item).await;
        print_preview_table(&rows);
        return Ok(());
    }

    // 2) 按 ID 精确查找
    let mut found: Vec<(String, Client, Item)> = Vec::new();
    for (name, client) in &clients {
        if let Ok(item) = client.item(target).await {
            found.push((name.clone(), client.clone(), item));
        }
    }
    if !found.is_empty() {
        let mut rows = Vec::new();
        for (name, client, item) in found {
            rows.extend(search_rows(&name, &client, item).await);
        }
        print_preview_table(&rows);
        return Ok(());
    }

    // 3) 当关键字搜索
    let mut hits: Vec<(i64, String, Client, Item)> = Vec::new();
    let mut failed = 0usize;
    for (name, client) in clients {
        match client.search(target, limit).await {
            Ok(items) => {
                for item in items {
                    let score = item.search_score(target);
                    hits.push((score, name.clone(), client.clone(), item));
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("⚠️  {name} 搜索失败: {e:#}");
            }
        }
    }

    // 跨服务器的结果统一按相关度排序，再展开成预览行。
    hits.sort_by_cached_key(|(score, name, _, item)| {
        (
            std::cmp::Reverse(*score),
            item.title().chars().count(),
            item.title().to_lowercase(),
            name.clone(),
        )
    });
    let mut rows: Vec<SearchRow> = Vec::new();
    for (_, name, client, item) in hits {
        rows.extend(search_rows(&name, &client, item).await);
    }

    if rows.is_empty() {
        if failed > 0 {
            bail!("所有服务器搜索失败");
        }
        println!("没有匹配 “{target}” 的条目");
        return Ok(());
    }

    print_preview_table(&rows);
    println!("\n去掉 --dry-run 即可选择并下载：embydown dl {target}");
    Ok(())
}

/// 预览表格的一行。
struct SearchRow {
    server: String,
    kind: String,
    year: Option<i32>,
    season: Option<i32>,
    version: String,
    size: Option<u64>,
    res: String,
    video: String,
    audio: String,
    container: String,
    name: String,
}

/// 「条目 + 选中的媒体源」引用。
type FileRef<'a> = (&'a Item, Option<&'a MediaSource>);
/// 一个版本分组：版本名 + 属于该版本的文件。
type VersionGroup<'a> = (String, Vec<FileRef<'a>>);
/// 剧集按「季 + 版本名」分组；版本名来自媒体源，单源集也能并入对应版本。
type SeasonVersionGroups<'a> = std::collections::BTreeMap<(i32, String), VersionGroup<'a>>;
type VersionGroups<'a> = std::collections::BTreeMap<i32, VersionGroup<'a>>;

/// 提取单个文件的体积、分辨率、编码、容器。
fn file_info(item: &Item) -> (Option<u64>, String, String, String, String) {
    file_info_source(item, None)
}

fn file_info_source(
    item: &Item,
    source: Option<&MediaSource>,
) -> (Option<u64>, String, String, String, String) {
    let ms = source.or_else(|| item.media_sources.as_ref().and_then(|s| s.first()));
    let size = ms.and_then(|m| m.size).or_else(|| item.size());
    let container = ms
        .and_then(|m| m.container.clone())
        .or_else(|| item.container.clone())
        .unwrap_or_default();
    let streams = ms
        .and_then(|m| m.media_streams.as_ref())
        .or(item.media_streams.as_ref());
    let video = streams.and_then(|ss| {
        ss.iter()
            .find(|s| s.stream_type.as_deref() == Some("Video"))
    });
    let audio = streams.and_then(|ss| {
        ss.iter()
            .find(|s| s.stream_type.as_deref() == Some("Audio"))
    });
    let res = video
        .and_then(|v| {
            v.width
                .map(|w| match w {
                    w if w >= 3800 => "2160p".to_string(),
                    w if w >= 2500 => "1440p".to_string(),
                    w if w >= 1900 => "1080p".to_string(),
                    w if w >= 1260 => "720p".to_string(),
                    w => format!("{w}w"),
                })
                .or_else(|| v.height.filter(|h| *h > 0).map(|h| format!("{h}p")))
        })
        .unwrap_or_default();
    let vcodec = video
        .and_then(|v| v.codec.clone())
        .unwrap_or_default();
    let acodec = audio
        .and_then(|a| a.codec.clone())
        .unwrap_or_default();
    (size, res, vcodec, acodec, container)
}

/// 构造预览行。剧集展开后按「季 × 版本」分行，各自汇总体积、编码和集数。
async fn search_rows(server: &str, client: &Client, item: Item) -> Vec<SearchRow> {
    if item.is_container() {
        let kinds = "Movie,Episode,Video,Audio,MusicVideo,Trailer";
        let rows = match client.children(&item.id, true, Some(kinds), None).await {
            Ok(children) => {
                let files: Vec<&Item> = children.iter().filter(|c| is_playable(c)).collect();
                if files.is_empty() {
                    vec![basic_row(server, &item)]
                } else if item.item_type.as_deref() == Some("Series") {
                    // 只有「同一集存在多个来源」的季才按版本拆行。
                    // 每集只有一个来源的季（单版本剧）保持一行，避免把片源差异
                    // （1080p / BDRip / BluRay）误当成多个版本。
                    let mut multi_seasons: std::collections::BTreeSet<i32> =
                        std::collections::BTreeSet::new();
                    let mut episode_counts: std::collections::HashMap<(i32, i32), usize> =
                        std::collections::HashMap::new();
                    for f in &files {
                        let season = f.season_no();
                        if f.media_sources
                            .as_deref()
                            .map(|s| s.len() > 1)
                            .unwrap_or(false)
                        {
                            multi_seasons.insert(season);
                        }
                        if f.episode_no() > 0 {
                            *episode_counts
                                .entry((season, f.episode_no()))
                                .or_insert(0) += 1;
                        }
                    }
                    for ((season, _), n) in &episode_counts {
                        if *n > 1 {
                            multi_seasons.insert(*season);
                        }
                    }

                    let mut groups: SeasonVersionGroups<'_> = std::collections::BTreeMap::new();
                    let mut season_labels: std::collections::BTreeMap<
                        i32,
                        std::collections::BTreeMap<String, usize>,
                    > = std::collections::BTreeMap::new();
                    for f in &files {
                        let season = f.season_no();
                        let sources = f.media_sources.as_deref().unwrap_or(&[]);
                        let entries: Vec<(Option<&MediaSource>, String)> = if sources.is_empty() {
                            vec![(None, String::new())]
                        } else {
                            sources
                                .iter()
                                .enumerate()
                                .map(|(i, src)| {
                                    // 多源条目用带兜底的版本名；单源条目提取不到就留空
                                    let label = if sources.len() > 1 {
                                        naming::version_label(f, src, i)
                                    } else {
                                        naming::version_label_opt(f, src).unwrap_or_default()
                                    };
                                    (Some(src), label)
                                })
                                .collect()
                        };
                        for (src, label) in entries {
                            *season_labels
                                .entry(season)
                                .or_default()
                                .entry(label.clone())
                                .or_insert(0) += 1;
                            if multi_seasons.contains(&season) {
                                push_version_group(&mut groups, season, label, (f, src));
                            } else {
                                push_version_group(&mut groups, season, String::new(), (f, src));
                            }
                        }
                    }

                    let season_labels: std::collections::BTreeMap<i32, String> = season_labels
                        .into_iter()
                        .map(|(season, counts)| (season, season_version_summary(&counts)))
                        .collect();

                    groups
                        .into_iter()
                        .map(|((season, _), (version, group))| {
                            let version = if multi_seasons.contains(&season) {
                                version
                            } else {
                                season_labels.get(&season).cloned().unwrap_or_default()
                            };
                            let (size, res, video, audio, container) = aggregate_refs(&group);
                            let year = item
                                .production_year
                                .or_else(|| group.iter().find_map(|(f, _)| f.production_year));
                            SearchRow {
                                server: server.to_string(),
                                kind: "Season".to_string(),
                                year,
                                season: Some(season),
                                version,
                                size,
                                res,
                                video,
                                audio,
                                container,
                                name: format!(
                                    "{}（{} 集）",
                                    item.title(),
                                    group_episode_count(&group)
                                ),
                            }
                        })
                        .collect()
                } else {
                    let mut groups: VersionGroups<'_> = std::collections::BTreeMap::new();
                    for f in &files {
                        match f.media_sources.as_deref() {
                            Some(srcs) if srcs.len() > 1 => {
                                for (i, src) in srcs.iter().enumerate() {
                                    let label = naming::version_label(f, src, i);
                                    groups
                                        .entry(i as i32)
                                        .or_insert_with(|| (label, Vec::new()))
                                        .1
                                        .push((f, Some(src)));
                                }
                            }
                            other => {
                                let src = other.and_then(|s| s.first());
                                groups
                                    .entry(-1)
                                    .or_insert_with(|| (String::new(), Vec::new()))
                                    .1
                                    .push((f, src));
                            }
                        }
                    }
                    groups
                        .into_iter()
                        .map(|(_, (version, group))| {
                            let (size, res, video, audio, container) = aggregate_refs(&group);
                            SearchRow {
                                server: server.to_string(),
                                kind: item.display_type().to_string(),
                                year: item.production_year,
                                season: None,
                                version,
                                size,
                                res,
                                video,
                                audio,
                                container,
                                name: format!("{}（{} 个视频）", item.title(), group.len()),
                            }
                        })
                        .collect()
                }
            }
            Err(e) => {
                eprintln!("⚠️  {server} 展开 {} 失败: {e:#}", item.title());
                vec![basic_row(server, &item)]
            }
        };
        // 温和限速
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        return rows;
    }

    let suffix = if item.item_type.as_deref() == Some("Episode") {
        format!(
            " [S{:02}E{:02}]",
            item.season_no(),
            item.episode_no()
        )
    } else {
        String::new()
    };
    let season = if item.item_type.as_deref() == Some("Episode") {
        Some(item.season_no())
    } else {
        None
    };
    let year = item.production_year;
    let kind = item.display_type().to_string();
    let name = format!("{}{suffix}", item.title());

    match item.media_sources.as_deref() {
        // 多版本：每个版本一行
        Some(sources) if sources.len() > 1 => sources
            .iter()
            .enumerate()
            .map(|(i, src)| {
                let (size, res, video, audio, container) = file_info_source(&item, Some(src));
                SearchRow {
                    server: server.to_string(),
                    kind: kind.clone(),
                    year,
                    season,
                    version: naming::version_label(&item, src, i),
                    size,
                    res,
                    video,
                    audio,
                    container,
                    name: name.clone(),
                }
            })
            .collect(),
        Some(sources) => {
            let src = sources.first();
            let (size, res, video, audio, container) = file_info_source(&item, src);
            vec![SearchRow {
                server: server.to_string(),
                kind,
                year,
                season,
                version: src
                    .and_then(|s| naming::version_label_opt(&item, s))
                    .unwrap_or_default(),
                size,
                res,
                video,
                audio,
                container,
                name,
            }]
        }
        None => vec![basic_row(server, &item)],
    }
}

fn basic_row(server: &str, item: &Item) -> SearchRow {
    let (size, res, video, audio, container) = file_info(item);
    SearchRow {
        server: server.to_string(),
        kind: item.display_type().to_string(),
        year: item.production_year,
        season: None,
        version: String::new(),
        size,
        res,
        video,
        audio,
        container,
        name: item.title(),
    }
}

/// 往「季 + 版本」分组里塞一个文件；版本名忽略大小写与多余空格参与分组。
fn push_version_group<'a>(
    groups: &mut SeasonVersionGroups<'a>,
    season: i32,
    label: String,
    file: FileRef<'a>,
) {
    let key = label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    groups
        .entry((season, key))
        .or_insert_with(|| (label, Vec::new()))
        .1
        .push(file);
}

/// 单版本季的 VERSION 展示：全部都有且一致时显示该标签，否则显示「多种片源」。
fn season_version_summary(counts: &std::collections::BTreeMap<String, usize>) -> String {
    let distinct: Vec<&String> = counts.keys().filter(|k| !k.is_empty()).collect();
    let unlabeled = counts.get("").copied().unwrap_or(0);
    match distinct.as_slice() {
        [] => String::new(),
        [only] if unlabeled == 0 => (*only).clone(),
        _ => "多种片源".to_string(),
    }
}

/// 组内不同集数；同一集重复出现的媒体源不重复计数。
fn group_episode_count(group: &[FileRef<'_>]) -> usize {
    let mut seen = std::collections::BTreeSet::new();
    let mut unknown = 0usize;
    for (f, _) in group {
        let e = f.episode_no();
        if e > 0 {
            seen.insert(e);
        } else {
            unknown += 1;
        }
    }
    seen.len() + unknown
}

/// 汇总一组「条目 + 媒体源」的体积、分辨率、编码与容器。
fn aggregate_refs(files: &[FileRef<'_>]) -> (Option<u64>, String, String, String, String) {
    let mut total = 0u64;
    let mut has_size = false;
    let mut resolutions = std::collections::BTreeSet::new();
    let mut videos = std::collections::BTreeSet::new();
    let mut audios = std::collections::BTreeSet::new();
    let mut containers = std::collections::BTreeSet::new();
    for (item, source) in files {
        let (s, r, v, a, c) = file_info_source(item, *source);
        if let Some(s) = s {
            total += s;
            has_size = true;
        }
        for (set, val) in [
            (&mut resolutions, r),
            (&mut videos, v),
            (&mut audios, a),
            (&mut containers, c),
        ] {
            if !val.is_empty() {
                set.insert(val);
            }
        }
    }
    (
        if has_size { Some(total) } else { None },
        join_set(resolutions),
        join_set(videos),
        join_set(audios),
        join_set(containers),
    )
}

fn join_set(set: std::collections::BTreeSet<String>) -> String {
    let vals: Vec<String> = set.into_iter().collect();
    if vals.len() > 3 {
        format!("{}+", vals[..3].join("/"))
    } else {
        vals.join("/")
    }
}

/// 为下载定位条目：--server > `名称:ID` > 在所有服务器上自动查找。
async fn pick_item(
    cfg: &Config,
    server: Option<&str>,
    target: &str,
    limit: u32,
) -> Result<(String, Client, Item)> {
    if cfg.is_empty() {
        bail!(
            "尚未登录任何服务器；请先运行 embydown login --server <URL> --user <账号> --password <密码>"
        );
    }
    if let Some(name) = server {
        let profile = cfg
            .get(name)
            .with_context(|| format!("没有名为 {name} 的服务器；已登录：{}", cfg.name_list()))?;
        let client = client_for(name, profile).await?;
        match client.item(target).await {
            Ok(item) => return Ok((name.to_string(), client, item)),
            Err(e) => {
                util::vlog(1, format!("  ↳ {name} 上未找到 ID {target}，尝试按名称搜索…"));
                util::vlog(2, format!("    {e:#}"));
            }
        }
        let found = search_clients(vec![(name.to_string(), client)], target, limit).await;
        if found.is_empty() {
            bail!("在 {name} 上没有找到 “{target}”");
        }
        return choose_item(found, target, cfg.servers.len());
    }

    if let Some((prefix, id)) = target.split_once(':')
        && let Some(profile) = cfg.get(prefix)
    {
        let client = client_for(prefix, profile).await?;
        let item = client
            .item(id)
            .await
            .with_context(|| format!("在 {prefix} 上获取条目 {id} 失败"))?;
        return Ok((prefix.to_string(), client, item));
    }

    let mut clients = Vec::new();
    for (name, profile) in &cfg.servers {
        match client_for(name, profile).await {
            Ok(c) => clients.push((name.clone(), c)),
            Err(e) => eprintln!("⚠️  {name} 连接失败: {e:#}"),
        }
    }
    if clients.is_empty() {
        bail!("所有服务器都连接失败");
    }

    // 先按 ID 在所有服务器上查找
    let results = join_all(clients.iter().map(|(name, client)| {
        let name = name.clone();
        let client = client.clone();
        async move {
            let res = client.item(target).await;
            (name, client, res)
        }
    }))
    .await;

    let mut found: Vec<(String, Client, Item)> = Vec::new();
    for (name, client, res) in results {
        match res {
            Ok(item) => found.push((name, client, item)),
            Err(e) => util::vlog(2, format!("  ↳ {name} 未找到 ID {target}: {e:#}")),
        }
    }
    if !found.is_empty() {
        return choose_item(found, target, cfg.servers.len());
    }

    // ID 找不到时，把参数当片名在所有服务器上搜索
    util::vlog(1, format!("  ↳ 按 ID 未找到，尝试在所有服务器搜索 “{target}”…"));
    let found = search_clients(clients, target, limit).await;
    if found.is_empty() {
        bail!("所有服务器上都没找到 “{target}”；可用 embydown dl {target} --dry-run 查看匹配结果");
    }
    choose_item(found, target, cfg.servers.len())
}

/// 在所有给定服务器上按关键字搜索（并发）。
async fn search_clients(
    clients: Vec<(String, Client)>,
    keyword: &str,
    limit: u32,
) -> Vec<(String, Client, Item)> {
    let keyword = keyword.to_string();
    let results = join_all(clients.into_iter().map(|(name, client)| {
        let keyword = keyword.clone();
        async move {
            let res = client.search(&keyword, limit).await;
            (name, client, res)
        }
    }))
    .await;

    let mut found = Vec::new();
    for (name, client, res) in results {
        match res {
            Ok(items) => {
                for item in items {
                    found.push((name.clone(), client.clone(), item));
                }
            }
            Err(e) => eprintln!("⚠️  {name} 搜索失败: {e:#}"),
        }
    }
    // 合并多服务器结果后按相关度排序，ID 找不到时的自动选择也依赖这个顺序。
    found.sort_by_cached_key(|(name, _, item)| {
        (
            std::cmp::Reverse(item.search_score(&keyword)),
            item.title().chars().count(),
            item.title().to_lowercase(),
            name.clone(),
        )
    });
    found
}

/// 从候选条目中选一个：单个自动使用，多个让用户选择。
fn choose_item(
    mut found: Vec<(String, Client, Item)>,
    target: &str,
    total_servers: usize,
) -> Result<(String, Client, Item)> {
    match found.len() {
        0 => bail!("所有服务器上都没找到 “{target}”"),
        1 => {
            let (name, client, item) = found.remove(0);
            if total_servers > 1 {
                println!("📡 使用服务器 {name}");
            } else {
                util::vlog(1, format!("📡 自动选择服务器 {name}"));
            }
            Ok((name, client, item))
        }
        _ => {
            println!("“{target}” 在 {} 个服务器/条目中匹配：", found.len());
            for (i, (name, _, item)) in found.iter().enumerate() {
                let size = item.size().map(format_bytes).unwrap_or_else(|| "-".into());
                let year = item
                    .production_year
                    .map(|y| y.to_string())
                    .unwrap_or_else(|| "-".into());
                let source = item
                    .media_sources
                    .as_ref()
                    .and_then(|s| s.first())
                    .map(|m| {
                        format!(
                            "{}{}",
                            m.name.clone().unwrap_or_default(),
                            m.container
                                .as_deref()
                                .map(|c| format!(" [{c}]"))
                                .unwrap_or_default()
                        )
                    })
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| format!("  {s}"))
                    .unwrap_or_default();
                println!(
                    "  [{i}] {:<24} {:<12} {:>5} {:>10}  {}{}",
                    name,
                    item.display_type(),
                    year,
                    size,
                    item.title(),
                    source
                );
            }
            let idx = prompt_choice(
                found.len(),
                "服务器",
                "非交互环境请用 --server <名称> 指定",
            )?;
            let (name, client, item) = found.remove(idx);
            println!("📡 已选择服务器 {name}");
            Ok((name, client, item))
        }
    }
}

fn prompt_choice(n: usize, what: &str, hint: &str) -> Result<usize> {
    print!("请选择{what} [0-{}]（回车取消）: ", n - 1);
    std::io::stdout().flush().ok();
    let mut line = String::new();
    let read = std::io::stdin()
        .read_line(&mut line)
        .context("读取选择失败")?;
    if read == 0 {
        bail!("没有读取到选择；{hint}");
    }
    let s = line.trim();
    if s.is_empty() {
        bail!("已取消");
    }
    let idx: usize = s.parse().with_context(|| format!("无效选择: {s}"))?;
    if idx >= n {
        bail!("选择超出范围: {idx}");
    }
    Ok(idx)
}

enum VersionPick {
    Index(usize),
    Name(String),
}

/// 处理多版本：`--version all` 展开全部；指定名称/序号时只保留包含该版本的集；
/// 未指定时列出所有版本让用户选择，绝不自动混着下。
fn apply_version_choice(items: &mut Vec<Item>, arg: Option<&str>) -> Result<()> {
    if !has_multiple_versions(items) {
        return Ok(());
    }

    match arg {
        Some(a) if a.eq_ignore_ascii_case("all") => {
            *items = expand_versions(std::mem::take(items));
        }
        Some(a) => {
            let pick = parse_version_arg(a);
            let versions = distinct_versions(items);
            let target = match &pick {
                VersionPick::Index(i) => versions.get(*i).cloned(),
                VersionPick::Name(needle) => {
                    let needle = needle.trim().to_lowercase();
                    versions
                        .iter()
                        .find(|v| v.to_lowercase() == needle)
                        .or_else(|| versions.iter().find(|v| v.to_lowercase().contains(&needle)))
                        .cloned()
                }
            };
            let target = match target {
                Some(t) => t,
                None => bail!(
                    "没有匹配 “{a}” 的版本；可用版本：{}",
                    if versions.is_empty() {
                        "（无）".to_string()
                    } else {
                        versions.join(" / ")
                    }
                ),
            };
            let (kept, skipped) = filter_to_version(std::mem::take(items), &target);
            if kept.is_empty() {
                bail!("版本 “{target}” 下没有可下载的条目");
            }
            if skipped > 0 {
                println!("ℹ️  版本 “{target}”：跳过 {skipped} 个不含该版本的条目");
            }
            *items = kept;
        }
        None => {
            let versions = version_summaries(items);
            match versions.len() {
                0 => {
                    eprintln!(
                        "⚠️  检测到多个版本但无法识别版本名；请用 --version 指定，或 --version all 全部下载"
                    );
                }
                1 => {
                    let target = versions[0].label.clone();
                    let (kept, skipped) = filter_to_version(std::mem::take(items), &target);
                    if skipped > 0 {
                        println!("ℹ️  版本 “{target}”：跳过 {skipped} 个不含该版本的条目");
                    }
                    *items = kept;
                }
                _ => {
                    let idx = prompt_version(&versions)?;
                    let target = versions[idx].label.clone();
                    let (kept, skipped) = filter_to_version(std::mem::take(items), &target);
                    if kept.is_empty() {
                        bail!("版本 “{target}” 下没有可下载的条目");
                    }
                    if skipped > 0 {
                        println!("ℹ️  版本 “{target}”：跳过 {skipped} 个不含该版本的条目");
                    }
                    *items = kept;
                }
            }
        }
    }
    Ok(())
}

/// 是否存在多版本：单个条目有多个媒体源，或同一部剧的同一集被拆成了多个条目。
fn has_multiple_versions(items: &[Item]) -> bool {
    if items.iter().any(|i| {
        i.media_sources
            .as_ref()
            .map(|s| s.len() > 1)
            .unwrap_or(false)
    }) {
        return true;
    }
    // 必须带上剧集标识：合集里不同剧的 S01E01 不是同一集的不同版本。
    let mut seen: std::collections::HashMap<(String, i32, i32), usize> =
        std::collections::HashMap::new();
    for item in items {
        if item.item_type.as_deref() == Some("Episode") {
            let e = item.episode_no();
            if e > 0 {
                let key = (series_key(item), item.season_no(), e);
                *seen.entry(key).or_insert(0) += 1;
            }
        }
    }
    seen.values().any(|n| *n > 1)
}

/// 同一部剧的标识（优先 SeriesId；部分服务器不返回时退到剧名）。
fn series_key(item: &Item) -> String {
    item.series_id
        .clone()
        .or_else(|| item.series_name.clone())
        .unwrap_or_default()
}

/// 与预览一致的版本名：多源条目用兜底名，单源条目只取能识别出的标签。
fn source_version_label(item: &Item, source: &MediaSource, index: usize) -> String {
    if item
        .media_sources
        .as_ref()
        .map(|s| s.len() > 1)
        .unwrap_or(false)
    {
        naming::version_label(item, source, index)
    } else {
        naming::version_label_opt(item, source).unwrap_or_default()
    }
}

/// 当前条目集合里出现过的版本名（忽略空标签，按名称排序去重）。
fn distinct_versions(items: &[Item]) -> Vec<String> {
    let mut versions: Vec<String> = Vec::new();
    for item in items {
        if let Some(sources) = item.media_sources.as_ref() {
            for (i, src) in sources.iter().enumerate() {
                let label = source_version_label(item, src, i);
                if !label.is_empty() && !versions.iter().any(|v| v.eq_ignore_ascii_case(&label)) {
                    versions.push(label);
                }
            }
        }
    }
    versions.sort_by_key(|v| v.to_lowercase());
    versions
}

/// 只保留包含指定版本的条目；单源集也按版本名参与匹配，不匹配则跳过。
fn filter_to_version(items: Vec<Item>, target: &str) -> (Vec<Item>, usize) {
    let mut kept = Vec::new();
    let mut skipped = 0usize;
    for mut item in items {
        let sources = item.media_sources.clone().unwrap_or_default();
        let found = sources
            .iter()
            .enumerate()
            .find(|(i, src)| source_version_label(&item, src, *i).eq_ignore_ascii_case(target));
        match found {
            Some((i, _)) => {
                apply_source(&mut item, &sources, i);
                item.version = Some(target.to_string());
                kept.push(item);
            }
            None => skipped += 1,
        }
    }
    (kept, skipped)
}

fn parse_version_arg(arg: &str) -> VersionPick {
    if let Ok(n) = arg.parse::<usize>() {
        VersionPick::Index(n.saturating_sub(1))
    } else {
        VersionPick::Name(arg.to_lowercase())
    }
}

#[derive(Default)]
struct VersionAcc {
    label: String,
    episodes: std::collections::BTreeSet<(String, i32, i32)>,
    singles: usize,
    is_episode: bool,
    size: u64,
    has_size: bool,
    res: String,
    container: String,
}

struct VersionSummary {
    label: String,
    count: usize,
    is_episode: bool,
    size: Option<u64>,
    res: String,
    container: String,
}

/// 汇总每个版本覆盖的集数/条目数与总体积，用于交互选择。
fn version_summaries(items: &[Item]) -> Vec<VersionSummary> {
    let mut accs: std::collections::HashMap<String, VersionAcc> =
        std::collections::HashMap::new();
    for item in items {
        let sources = item.media_sources.as_deref().unwrap_or(&[]);
        for (i, src) in sources.iter().enumerate() {
            let label = source_version_label(item, src, i);
            if label.is_empty() {
                continue;
            }
            let acc = accs
                .entry(label.to_lowercase())
                .or_insert_with(|| VersionAcc {
                    label,
                    ..Default::default()
                });
            if item.item_type.as_deref() == Some("Episode") && item.episode_no() > 0 {
                acc.is_episode = true;
                acc.episodes
                    .insert((series_key(item), item.season_no(), item.episode_no()));
            } else {
                acc.singles += 1;
            }
            if let Some(size) = src.size {
                acc.size += size;
                acc.has_size = true;
            }
            if acc.res.is_empty() || acc.container.is_empty() {
                let (_, res, _, _, container) = file_info_source(item, Some(src));
                if acc.res.is_empty() {
                    acc.res = res;
                }
                if acc.container.is_empty() {
                    acc.container = container;
                }
            }
        }
    }
    let mut out: Vec<VersionSummary> = accs
        .into_values()
        .map(|a| VersionSummary {
            label: a.label,
            count: a.episodes.len() + a.singles,
            is_episode: a.is_episode,
            size: if a.has_size { Some(a.size) } else { None },
            res: a.res,
            container: a.container,
        })
        .collect();
    out.sort_by_key(|v| v.label.to_lowercase());
    out
}

/// 列出所有版本让用户选一个；非交互环境直接报错，避免误下多个版本。
fn prompt_version(versions: &[VersionSummary]) -> Result<usize> {
    println!("检测到 {} 个版本，请选择要下载的一个：", versions.len());
    for (i, v) in versions.iter().enumerate() {
        let size = v.size.map(format_bytes).unwrap_or_else(|| "-".into());
        let count = if v.is_episode {
            format!("{} 集", v.count)
        } else {
            format!("{} 个文件", v.count)
        };
        println!(
            "  [{}] {:<32} {:>8} {:>10}  {} {}",
            i + 1,
            v.label,
            count,
            size,
            dash(&v.res),
            dash(&v.container)
        );
    }
    print!("请选择版本 [1-{}]（回车取消）: ", versions.len());
    std::io::stdout().flush().ok();
    let mut line = String::new();
    let read = std::io::stdin()
        .read_line(&mut line)
        .context("读取选择失败")?;
    if read == 0 {
        bail!("没有读取到选择；非交互环境请用 --version <名称|序号> 指定");
    }
    let s = line.trim();
    if s.is_empty() {
        bail!("已取消");
    }
    let n: usize = s.parse().with_context(|| format!("无效选择: {s}"))?;
    if n == 0 || n > versions.len() {
        bail!("选择超出范围: {n}（有效范围 1-{}）", versions.len());
    }
    Ok(n - 1)
}

fn apply_source(item: &mut Item, sources: &[MediaSource], idx: usize) {
    let source = sources[idx].clone();
    let label = source_version_label(item, &source, idx);
    if source.media_streams.is_some() {
        item.media_streams = source.media_streams.clone();
    }
    if source.container.is_some() {
        item.container = source.container.clone();
    }
    // 只有“从多个源里挑一个”才禁止版本无关的回退接口；
    // 单源条目虽然也带版本名，但 /Items/{id}/Download 仍是安全的。
    item.source_locked = sources.len() > 1;
    item.media_sources = Some(vec![source]);
    item.version = Some(label);
}

fn expand_versions(items: Vec<Item>) -> Vec<Item> {
    let mut out = Vec::new();
    for mut item in items {
        let sources = item.media_sources.clone().unwrap_or_default();
        if sources.len() > 1 {
            // 不同媒体源可能提取出相同的版本名；追加序号保证每个版本
            // 落到不同的目录，否则后一个会覆盖前一个。
            let mut used: Vec<String> = Vec::new();
            for i in 0..sources.len() {
                let mut it = item.clone();
                apply_source(&mut it, &sources, i);
                let mut label = it.version.clone().unwrap_or_default();
                if label.is_empty() {
                    label = naming::version_label(&item, &sources[i], i);
                }
                if used.iter().any(|u| u.eq_ignore_ascii_case(&label)) {
                    label = format!("{label} ({})", i + 1);
                }
                used.push(label.clone());
                it.version = Some(label);
                it.version_scoped = true;
                out.push(it);
            }
        } else {
            // 多版本剧里的单源集也带上自己的版本名，避免和版本目录混在一起
            if let Some(src) = sources.first() {
                let label = source_version_label(&item, src, 0);
                if !label.is_empty() {
                    item.version = Some(label);
                    item.media_sources = Some(vec![src.clone()]);
                }
            }
            item.version_scoped = true;
            out.push(item);
        }
    }
    out
}

/// 把已定位到的条目展开成可下载条目列表。
async fn resolve_item(client: &Client, root: Item, seasons: &[i32]) -> Result<Vec<Item>> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    if root.is_container() {
        let kinds = "Movie,Episode,Video,Audio,MusicVideo,Trailer";
        // 按层并行展开。部分服务器（如 Emby 4.9）在媒体库层会忽略
        // Recursive / IncludeItemTypes，只返回 Series，需要继续向下钻到分集。
        let mut frontier = vec![root];
        seen.insert(frontier[0].id.clone());
        let mut expanded = 0usize;
        let mut next_log = 100usize;
        while !frontier.is_empty() {
            let results = futures_util::stream::iter(frontier.into_iter().map(|node| {
                let client = client.clone();
                async move {
                    let res = client.children(&node.id, true, Some(kinds), None).await;
                    // 温和限速，避免触发服务器的 429 防滥用策略
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    (node, res)
                }
            }))
            .buffer_unordered(2)
            .collect::<Vec<_>>()
            .await;

            let mut next = Vec::new();
            for (node, res) in results {
                expanded += 1;
                let children = res.with_context(|| {
                    format!("展开 {}（{}）失败", node.title(), node.display_type())
                })?;
                // 部分服务器列表接口不返回 SeriesName / ParentIndexNumber，
                // 从当前容器继承，避免把分集命名成单集标题或 Season 00。
                let inherited_series = if node.item_type.as_deref() == Some("Series") {
                    Some(node.title())
                } else {
                    node.series_name.clone()
                };
                let inherited_season = if node.item_type.as_deref() == Some("Season") {
                    node.index_number
                } else {
                    None
                };
                let inherited_series_year = if node.item_type.as_deref() == Some("Series") {
                    node.production_year
                } else {
                    node.series_year
                };
                for mut c in children {
                    if !seen.insert(c.id.clone()) {
                        continue;
                    }
                    let has_series = c
                        .series_name
                        .as_ref()
                        .map(|s| !s.trim().is_empty())
                        .unwrap_or(false);
                    if !has_series {
                        c.series_name = inherited_series.clone();
                    }
                    if c.parent_index_number.is_none() {
                        c.parent_index_number = inherited_season;
                    }
                    if c.series_year.is_none() {
                        c.series_year = inherited_series_year;
                    }
                    if c.is_container() {
                        next.push(c);
                    } else if is_playable(&c) {
                        out.push(c);
                    }
                }
            }
            if expanded >= next_log {
                util::vlog(
                    1,
                    format!(
                        "  ↳ 已展开 {expanded} 个容器，得到 {} 个可下载条目",
                        out.len()
                    ),
                );
                next_log = expanded + 100;
            }
            frontier = next;
        }
        if expanded > 1 {
            util::vlog(1, format!("  ↳ 共展开 {expanded} 个容器"));
        }
    } else {
        out.push(root);
    }

    // 季过滤 + 过滤掉没有媒体源的占位条目
    out.retain(|c| {
        if !seasons.is_empty()
            && c.item_type.as_deref() == Some("Episode")
            && !seasons.contains(&c.season_no())
        {
            return false;
        }
        is_playable(c)
    });
    Ok(out)
}

/// 是否有可下载的媒体源。
fn is_playable(item: &Item) -> bool {
    item.is_video() || item.media_sources.as_ref().map(|s| !s.is_empty()).unwrap_or(false)
}

/// 纯直链 → 伪造一个 Item，让下载器复用同一套分块逻辑。
fn direct_item(url: &str) -> Item {
    let parsed = reqwest::Url::parse(url).ok();
    let segments: Vec<String> = parsed
        .as_ref()
        .and_then(|u| u.path_segments())
        .map(|segs| segs.map(percent_decode).collect())
        .unwrap_or_default();
    let last = segments.last().cloned().unwrap_or_default();
    // Emby 风格：/Videos/{id}/stream、/Audio/{id}/stream、/Items/{id}/Download。
    // 这些路径的末段是 `stream`/`Download`，不能拿来当文件名。
    let media_id = segments
        .iter()
        .position(|s| matches!(s.as_str(), "Videos" | "Audio" | "Items"))
        .and_then(|i| segments.get(i + 1))
        .filter(|s| !s.is_empty())
        .cloned();
    let query_ext = parsed
        .as_ref()
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k.eq_ignore_ascii_case("Container"))
                .map(|(_, v)| v.to_string())
        })
        .filter(|v| is_plain_ext(v));
    let (stem, ext) = match last.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && is_plain_ext(e) => {
            (s.to_string(), e.to_ascii_lowercase())
        }
        _ => (last.clone(), String::new()),
    };
    let generic = matches!(
        stem.to_ascii_lowercase().as_str(),
        "" | "stream" | "download" | "file" | "original"
    );
    // 取不到有意义的名字时用条目 ID：不同 ID 的直链不会再互相覆盖/被跳过。
    let name = if generic {
        media_id.unwrap_or_else(|| format!("direct-{:016x}", stable_hash(url)))
    } else {
        stem
    };
    let ext = if !ext.is_empty() {
        ext
    } else if let Some(q) = query_ext {
        q.to_ascii_lowercase()
    } else {
        "bin".to_string()
    };
    Item {
        id: "direct".to_string(),
        name: Some(name),
        item_type: Some("Video".to_string()),
        container: Some(ext),
        direct_url: Some(url.to_string()),
        ..Default::default()
    }
}

fn is_plain_ext(s: &str) -> bool {
    !s.is_empty() && s.len() <= 5 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// FNV-1a：稳定、无依赖，用于给无法从路径推断名字的 URL 生成短标识。
fn stable_hash(s: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| -> Option<u8> {
                match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    b'A'..=b'F' => Some(c - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn print_table(items: &[Item]) {
    println!(
        "{:<32}  {:<13} {:>6}  {:>10}  NAME",
        "ID", "TYPE", "YEAR", "SIZE"
    );
    println!("{}", "-".repeat(100));
    for it in items {
        println!("{}", table_row(it));
    }
    println!("\n共 {} 条", items.len());
}

/// 预览结果带服务器与文件信息（体积、分辨率、编码、容器），剧集按季分行。
fn print_preview_table(rows: &[SearchRow]) {
    let mut table = Table::new(&[
        "SERVER", "TYPE", "YEAR", "SEASON", "VERSION", "SIZE", "RES", "VCODEC", "ACODEC", "EXT",
        "NAME",
    ]);
    table = table.align(2, Align::Right).align(5, Align::Right);

    for r in rows {
        table.row(vec![
            r.server.clone(),
            r.kind.clone(),
            format_year(r.year),
            season_label(r.season),
            dash(&r.version).to_string(),
            r.size.map(format_bytes).unwrap_or_else(|| "-".into()),
            dash(&r.res).to_string(),
            dash(&r.video).to_string(),
            dash(&r.audio).to_string(),
            dash(&r.container).to_string(),
            r.name.clone(),
        ]);
    }

    println!("{}", table.render());
    println!("\n共 {} 条", rows.len());
}

fn season_label(season: Option<i32>) -> String {
    match season {
        Some(n) => format!("Season {:02}", n.max(0)),
        None => "-".into(),
    }
}

fn dash(s: &str) -> &str {
    if s.is_empty() {
        "-"
    } else {
        s
    }
}

fn table_row(it: &Item) -> String {
    let size = it.size().map(format_bytes).unwrap_or_else(|| "-".into());
    let year = format_year(it.production_year);
    let season_ep = match (it.season_no(), it.episode_no()) {
        (0, 0) => String::new(),
        (s, e) => format!(" [S{:02}E{:02}]", s, e),
    };
    format!(
        "{:<32}  {:<13} {:>6}  {:>10}  {}{}",
        it.id,
        it.display_type(),
        year,
        size,
        it.title(),
        season_ep
    )
}

/// Emby 偶尔返回 0/1 这类无效年份，统一显示为 “-”。
fn format_year(year: Option<i32>) -> String {
    year.filter(|y| *y >= 1800)
        .map(|y| y.to_string())
        .unwrap_or_else(|| "-".into())
}

fn print_detail(item: &Item) {
    println!("名称      : {}", item.title());
    println!("ID        : {}", item.id);
    println!("类型      : {}", item.display_type());
    if let Some(mt) = &item.media_type {
        println!("媒体类型  : {mt}");
    }
    if let Some(s) = &item.series_name {
        println!("所属剧集  : {s}");
    }
    if item.item_type.as_deref() == Some("Episode") {
        println!("季/集     : S{:02}E{:02}", item.season_no(), item.episode_no());
    }
    if let Some(y) = item.production_year {
        println!("年份      : {y}");
    }
    if let Some(t) = item.run_time_ticks {
        println!("时长      : {}", format_ticks(t));
    }
    if let Some(c) = &item.container {
        println!("容器      : {c}");
    }
    if let Some(s) = item.size() {
        println!("大小      : {}", format_bytes(s));
    }
    if let Some(p) = &item.path {
        println!("服务器路径: {p}");
    }
    if let Some(ms) = &item.media_sources {
        for (i, s) in ms.iter().enumerate() {
            println!(
                "媒体源 #{} : {} ({}, {})",
                i,
                s.name.clone().unwrap_or_else(|| "-".into()),
                s.container.clone().unwrap_or_else(|| "-".into()),
                s.size.map(format_bytes).unwrap_or_else(|| "-".into())
            );
        }
    }
    if let Some(streams) = &item.media_streams {
        for s in streams {
            println!(
                "流 #{}     : {:<9} {:<8} {}x{} {}",
                s.index.unwrap_or(0),
                s.stream_type.clone().unwrap_or_default(),
                s.codec.clone().unwrap_or_default(),
                s.width.unwrap_or(0),
                s.height.unwrap_or(0),
                s.language.clone().unwrap_or_default()
            );
        }
    }
    if let Some(o) = &item.overview {
        let brief: String = o.chars().take(200).collect();
        if !brief.trim().is_empty() {
            println!("\n简介      : {brief}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_padding_counts_cjk_as_two_columns() {
        let padded = pad_display("（直链）", 16);
        assert_eq!(unicode_width::UnicodeWidthStr::width(padded.as_str()), 16);
        assert_eq!(padded.trim_end(), "（直链）");

        // 已经达到或超过宽度时不再追加空格。
        assert_eq!(pad_display("media.micu.hk", 8), "media.micu.hk");
    }

    fn ep(id: &str, season: i32, num: i32, labels: &[&str]) -> Item {
        let sources: Vec<MediaSource> = labels
            .iter()
            .enumerate()
            .map(|(i, label)| MediaSource {
                id: Some(format!("{id}-{i}")),
                name: Some((*label).to_string()),
                container: Some("mkv".into()),
                ..Default::default()
            })
            .collect();
        Item {
            id: id.into(),
            name: Some(format!("第{num}集")),
            item_type: Some("Episode".into()),
            series_name: Some("测试剧".into()),
            parent_index_number: Some(season),
            index_number: Some(num),
            media_sources: Some(sources),
            ..Default::default()
        }
    }

    #[test]
    fn version_list_is_ordered_and_deduplicated() {
        let items = vec![
            ep("a", 1, 1, &["Nekomoe", "ANi"]),
            ep("b", 1, 2, &["ANi", "Nekomoe"]),
        ];
        assert_eq!(distinct_versions(&items), vec!["ANi", "Nekomoe"]);
    }

    #[test]
    fn filter_merges_single_source_episodes_into_version() {
        let items = vec![
            ep("a", 1, 1, &["ANi", "Nekomoe", "Sakurato"]),
            ep("b", 1, 2, &["ANi", "Nekomoe", "Sakurato"]),
            // 只有单源的集也要能并入 Nekomoe
            ep("c", 1, 3, &["Nekomoe"]),
        ];
        let (kept, skipped) = filter_to_version(items, "Nekomoe");
        assert_eq!(skipped, 0);
        assert_eq!(kept.len(), 3);
        assert!(kept
            .iter()
            .all(|i| i.version.as_deref() == Some("Nekomoe")));
        assert_eq!(kept[2].media_sources.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn filter_skips_episodes_without_the_version() {
        let items = vec![
            ep("a", 1, 1, &["ANi", "Nekomoe"]),
            ep("b", 1, 2, &["ANi", "Nekomoe"]),
            ep("c", 1, 3, &["Nekomoe"]),
        ];
        let (kept, skipped) = filter_to_version(items, "ANi");
        assert_eq!(skipped, 1);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|i| {
            let sources = i.media_sources.as_ref().unwrap();
            sources.len() == 1 && sources[0].name.as_deref() == Some("ANi")
        }));
    }

    #[test]
    fn only_multi_source_items_are_marked_as_locked() {
        let items = vec![
            ep("a", 1, 1, &["ANi", "Nekomoe"]),
            ep("c", 1, 2, &["Nekomoe"]),
        ];
        let (kept, _) = filter_to_version(items, "Nekomoe");
        assert!(kept[0].source_locked, "多源条目锁定后不能用版本无关的回退接口");
        assert!(!kept[1].source_locked, "单源条目的回退接口仍然安全");
    }

    #[test]
    fn boxset_with_different_series_is_not_multiple_versions() {
        let mut a = ep("a", 1, 1, &["1080p"]);
        a.series_name = Some("剧A".into());
        let mut b = ep("b", 1, 1, &["1080p"]);
        b.series_name = Some("剧B".into());
        assert!(!has_multiple_versions(&[a, b]));
    }

    #[test]
    fn expand_versions_labels_single_source_episodes() {
        let items = vec![
            ep("a", 1, 1, &["ANi", "Nekomoe"]),
            ep("c", 1, 2, &["Nekomoe"]),
        ];
        let out = expand_versions(items);
        assert_eq!(out.len(), 3);
        let labels: Vec<&str> = out
            .iter()
            .map(|i| i.version.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(labels, vec!["ANi", "Nekomoe", "Nekomoe"]);
        // 单源集仍然只有一个媒体源
        assert_eq!(out[2].media_sources.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn version_summaries_count_episodes_across_seasons() {
        let items = vec![
            ep("a", 1, 1, &["ANi", "Nekomoe"]),
            ep("b", 1, 2, &["ANi", "Nekomoe"]),
            ep("c", 2, 1, &["ANi"]),
        ];
        let vs = version_summaries(&items);
        assert_eq!(vs.len(), 2);
        assert_eq!(vs[0].label, "ANi");
        assert_eq!(vs[0].count, 3);
        assert!(vs[0].is_episode);
        assert_eq!(vs[1].label, "Nekomoe");
        assert_eq!(vs[1].count, 2);
    }

    #[test]
    fn detects_versions_split_into_separate_items() {
        // 同一集出现两次、但每条都只有一个媒体源
        let items = vec![ep("a", 1, 1, &["ANi"]), ep("b", 1, 1, &["Nekomoe"])];
        assert!(has_multiple_versions(&items));
        let (kept, skipped) = filter_to_version(items, "Nekomoe");
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "b");
        assert_eq!(skipped, 1);
    }

    #[test]
    fn direct_link_names_use_media_id_instead_of_generic_segment() {
        let stream = direct_item("http://host:8096/Videos/abc123/stream?static=true&api_key=k");
        assert_eq!(stream.title(), "abc123");
        assert_eq!(stream.ext(), "bin");

        let other = direct_item("http://host:8096/Videos/def456/stream?static=true");
        assert_ne!(stream.title(), other.title());
    }

    #[test]
    fn direct_link_names_keep_real_filenames_and_container_hints() {
        let file = direct_item("http://host/media/movie.mkv");
        assert_eq!(file.title(), "movie");
        assert_eq!(file.ext(), "mkv");

        let hinted = direct_item("http://host/Videos/9/stream?static=false&Container=mp4");
        assert_eq!(hinted.title(), "9");
        assert_eq!(hinted.ext(), "mp4");
    }

    #[test]
    fn direct_link_names_fall_back_to_stable_hash() {
        let a = direct_item("http://host/");
        let b = direct_item("http://host/other/");
        assert!(a.title().starts_with("direct-"));
        assert_ne!(a.title(), b.title());
        // 同一条 URL 的名字必须稳定，才能续传/跳过。
        assert_eq!(a.title(), direct_item("http://host/").title());
    }

    #[test]
    fn expand_versions_disambiguates_identical_labels() {
        let items = vec![ep("a", 1, 1, &["Same", "Same"])];
        let out = expand_versions(items);
        let labels: Vec<&str> = out
            .iter()
            .map(|i| i.version.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(labels, vec!["Same", "Same (2)"]);
        assert!(out.iter().all(|i| i.version_scoped));
    }
}
