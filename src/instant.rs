//! 秒传测试：只下载剧集的第一集，移动到最终文件夹，并通过 CD2 上传任务判断
//! 是真正的秒传还是实际传输。
//!
//! CD2 的挂载写入先落到本地缓存，FUSE 返回成功并不代表云端已有文件；因此
//! 这里查询 CD2 的 gRPC 上传任务（GetUploadFileList），观察该文件的
//! `status` / `transferedBytes`：
//! - `Skipped`，或 `Finish` 且传输字节远小于文件大小 → 秒传；
//! - `Transfer` 且已传输数据 → 普通上传；
//! - 观察窗口内无法确定 → 无法确认。

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};

use crate::cd2::{self, UploadStatus, UploadTask};
use crate::config;
use crate::download::{self, DownloadOpts};
use crate::naming;
use crate::util::{format_bytes, parse_rate};

const EXDEV: i32 = 18;

#[derive(Debug, Clone)]
pub struct Options {
    /// 命令行显式覆盖；None/空表示使用环境变量或配置文件。
    pub cd2_url: Option<String>,
    pub cd2_token: Option<String>,
    /// 观察上传任务的时间窗口。
    pub window: Duration,
    /// 保留最终文件夹中的测试目录（默认测试后删除）。
    pub keep: bool,
    pub temp_dir: Option<PathBuf>,
    pub final_dir: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            cd2_url: None,
            cd2_token: None,
            window: Duration::from_secs(20),
            keep: false,
            temp_dir: None,
            final_dir: None,
        }
    }
}

/// CLI 入口：先按 ID/名称定位条目，再执行测试。
pub async fn run(
    target: &str,
    server: Option<&str>,
    limit: u32,
    options: Options,
) -> Result<()> {
    let cfg = config::load_optional()?.unwrap_or_default();
    if target.starts_with("http://") || target.starts_with("https://") {
        let client = crate::emby::Client::new("http://127.0.0.1", false)?;
        let root = crate::direct_item(target);
        return run_item("（直链）".to_string(), client, root, &cfg.settings, options).await;
    }
    let (server_name, client, root) =
        crate::pick_item(&cfg, server, target, limit).await?;
    run_item(server_name, client, root, &cfg.settings, options).await
}

/// 交互模式入口：已经选好服务器与条目。
pub async fn run_item(
    server_name: String,
    client: crate::emby::Client,
    root: crate::model::Item,
    settings: &config::Settings,
    options: Options,
) -> Result<()> {
    let temp_root = resolve_dir(options.temp_dir.clone(), &settings.temp_dir, "./downloads")?;
    let final_root = match options.final_dir.clone() {
        Some(path) => crate::util::expand_tilde(&path)?,
        None => {
            let configured = settings.final_dir.trim();
            if configured.is_empty() {
                bail!(
                    "未设置最终文件夹：请用 --final 指定，或在交互模式「下载设置」中设置"
                );
            }
            crate::util::expand_tilde(Path::new(configured))?
        }
    };
    crate::review::ensure_separate(&temp_root, &final_root)?;

    println!("\n=== 秒传测试 ===");
    println!("服务器：{server_name}");
    println!("临时文件夹：{}", temp_root.display());
    println!("最终文件夹：{}", final_root.display());

    println!("\n正在解析 “{}” …", root.title());
    let resolved = crate::resolve_item(&client, root.clone(), &[])
        .await
        .with_context(|| format!("解析 {} 失败", root.title()))?;
    let mut item = first_episode(resolved)?;
    let label = item_label(&item);
    println!("已选择第一集：{label}");
    if let Some(sources) = item.media_sources.clone()
        && !sources.is_empty()
    {
        // 同一集有多个媒体源时优先用体积最大的版本，测试信号更明确。
        let idx = sources
            .iter()
            .enumerate()
            .max_by_key(|(_, source)| source.size.unwrap_or(0))
            .map(|(index, _)| index)
            .unwrap_or(0);
        crate::apply_source(&mut item, &sources, idx);
    }

    let pattern = settings.pattern.trim();
    let pattern = if pattern.is_empty() {
        naming::default_pattern(&item).to_string()
    } else {
        pattern.to_string()
    };
    let rel = naming::render(&pattern, &item, &item.ext());
    let file_name = Path::new(&rel)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "test-file".into());

    // 先确认 CD2 API 可用，避免白白下载一集。
    let cd2_config = cd2::Config::resolve(
        options.cd2_url.as_deref(),
        options.cd2_token.as_deref(),
        &settings.cd2_url,
        &settings.cd2_token,
    );
    println!("\n正在连接 CD2：{}", cd2_config.url);
    let cd2_client = cd2::Client::new(&cd2_config)?;
    let before = cd2_client
        .upload_tasks()
        .await
        .context("无法读取 CD2 上传任务；请检查 CD2 是否运行，以及是否需要 CLOUDDRIVE_API_TOKEN / --cd2-token")?;
    let known_keys: HashSet<String> = before
        .iter()
        .filter(|task| task_matches(task, &rel, &file_name, 0))
        .map(|task| task.key.clone())
        .collect();
    println!("CD2 已连接（当前 {} 个上传任务）", before.len());

    let test_root = temp_root.join(format!(
        ".embydown-instant-test-{}-{}",
        std::process::id(),
        now_secs()
    ));
    fs::create_dir_all(&test_root)
        .with_context(|| format!("创建测试目录失败：{}", test_root.display()))?;
    if let Some(parent) = test_root.join(&rel).parent() {
        fs::create_dir_all(parent)?;
    }

    let opts = build_opts(settings, &test_root)?;
    let src = test_root.join(&rel);
    println!("\n↓ 正在下载第一集到临时文件夹…");
    let download_started = Instant::now();
    let stats = download::execute(&client, vec![item.clone()], opts)
        .await
        .context("下载第一集失败")?;
    let download_elapsed = download_started.elapsed();
    if stats.ok == 0 || !src.exists() {
        let _ = fs::remove_dir_all(&test_root);
        bail!("下载失败或文件不存在：{}", src.display());
    }
    let size = fs::metadata(&src)
        .with_context(|| format!("读取下载文件失败：{}", src.display()))?
        .len();
    println!(
        "✓ 下载完成：{}（耗时 {}）",
        format_bytes(size),
        format_duration(download_elapsed)
    );

    // 在最终文件夹下建一次性测试目录，避免覆盖真实媒体库里的同名剧集。
    let final_test_dir = final_root.join(format!(
        "_embydown_instant_test_{}-{}",
        now_secs(),
        std::process::id()
    ));
    let dest = final_test_dir.join(&rel);
    println!("\n→ 移动到最终文件夹（测试目录）：{}", dest.display());
    let move_started = Instant::now();
    let copied = match move_file(&src, &dest) {
        Ok(copied) => copied,
        Err(e) => {
            println!(
                "⚠️  移动失败：{e:#}\n   下载文件保留在：{}",
                src.display()
            );
            return Err(e);
        }
    };
    let move_elapsed = move_started.elapsed();
    println!(
        "✓ 已移动到最终文件夹（{}，本地写入耗时 {}）",
        if copied { "跨文件系统复制" } else { "rename" },
        format_duration(move_elapsed)
    );
    let _ = fs::remove_dir_all(&test_root);

    let observation = observe(
        &cd2_client,
        &rel,
        &file_name,
        size,
        &known_keys,
        options.window,
    )
    .await;
    print_observation(&observation, size, &dest);

    if options.keep {
        println!(
            "\n测试目录已保留，可手动检查：{}",
            final_test_dir.display()
        );
    } else if observation.verdict == Verdict::Unknown {
        println!(
            "\n结果无法确认，测试目录已保留：{}",
            final_test_dir.display()
        );
        println!("可增大 --window 后重试，或手动检查后删除该目录。");
    } else {
        match fs::remove_dir_all(&final_test_dir) {
            Ok(()) => println!("\n🧹 已清理测试目录"),
            Err(e) => println!(
                "\n⚠️  清理测试目录失败：{e:#}\n   请手动删除：{}",
                final_test_dir.display()
            ),
        }
    }
    Ok(())
}

/// 从解析结果中挑出第一集；同一集有多个版本时选体积最大的。
fn first_episode(mut items: Vec<crate::model::Item>) -> Result<crate::model::Item> {
    items.retain(|item| {
        item.is_video()
            || item
                .media_sources
                .as_ref()
                .map(|sources| !sources.is_empty())
                .unwrap_or(false)
    });
    if items.is_empty() {
        bail!("没有可下载的条目（可能是空剧集或没有播放权限）");
    }
    items.sort_by_key(|item| {
        let episode = item.item_type.as_deref() == Some("Episode");
        let special = episode && item.season_no() == 0;
        (
            special,
            item.season_no(),
            item.episode_no(),
            std::cmp::Reverse(item_max_size(item)),
            item.title(),
        )
    });
    Ok(items.remove(0))
}

fn item_max_size(item: &crate::model::Item) -> u64 {
    item.media_sources
        .as_ref()
        .and_then(|sources| sources.iter().filter_map(|source| source.size).max())
        .or_else(|| item.size())
        .unwrap_or(0)
}

fn item_label(item: &crate::model::Item) -> String {
    let mut label = item.title();
    if item.item_type.as_deref() == Some("Episode") {
        label = format!(
            "{} S{:02}E{:02}",
            item.series_name.as_deref().unwrap_or(&label),
            item.season_no(),
            item.episode_no()
        );
        if item.title() != item.series_name.as_deref().unwrap_or("") {
            label = format!("{label} · {}", item.title());
        }
    }
    if let Some(size) = item.size() {
        label = format!("{label}（{}）", format_bytes(size));
    }
    label
}

fn resolve_dir(override_path: Option<PathBuf>, configured: &str, default: &str) -> Result<PathBuf> {
    let path = match override_path {
        Some(path) => path,
        None => {
            let configured = configured.trim();
            PathBuf::from(if configured.is_empty() {
                default
            } else {
                configured
            })
        }
    };
    crate::util::expand_tilde(&path)
}

fn build_opts(settings: &config::Settings, out: &Path) -> Result<DownloadOpts> {
    let limit = settings.limit.trim();
    let limit_rate = if limit.is_empty() {
        None
    } else {
        Some(parse_rate(limit).with_context(|| format!("无法解析限速值：{limit}"))?)
    };
    Ok(DownloadOpts {
        out: out.to_path_buf(),
        pattern: Some(settings.pattern.clone()).filter(|pattern| !pattern.trim().is_empty()),
        connections: settings.connections.clamp(1, 32),
        jobs: 1,
        limit_rate,
        resume: true,
        overwrite: true,
        subs: false,
        dry_run: false,
        fixed_dest: None,
    })
}

fn move_file(src: &Path, dest: &Path) -> Result<bool> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目标目录失败：{}", parent.display()))?;
    }
    match fs::rename(src, dest) {
        Ok(()) => Ok(false),
        Err(e) if e.raw_os_error() == Some(EXDEV) => {
            copy_atomic(src, dest)?;
            fs::remove_file(src)
                .with_context(|| format!("删除临时文件失败：{}", src.display()))?;
            Ok(true)
        }
        Err(e) => Err(e).with_context(|| {
            format!(
                "移动 {} → {} 失败",
                src.display(),
                dest.display()
            )
        }),
    }
}

fn copy_atomic(src: &Path, dest: &Path) -> Result<()> {
    let total = fs::metadata(src).map(|meta| meta.len()).unwrap_or(0);
    let bar = if total == 0 {
        None
    } else {
        let bar = ProgressBar::new(total);
        bar.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {bytes}/{total_bytes} {bar:32.cyan/blue} {percent}%  {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_bar()),
        );
        bar.set_message("写入最终文件夹（本地缓存）");
        Some(bar)
    };
    let parent = dest
        .parent()
        .with_context(|| format!("无法确定目标目录：{}", dest.display()))?;
    fs::create_dir_all(parent)?;
    let name = dest
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let temp = parent.join(format!(".{name}.embydown-instant-{}", std::process::id()));

    let result = (|| -> Result<()> {
        let mut input = File::open(src)
            .with_context(|| format!("打开源文件失败：{}", src.display()))?;
        let mut output = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
            .with_context(|| format!("创建目标临时文件失败：{}", temp.display()))?;
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            let read = input.read(&mut buffer).context("读取源文件失败")?;
            if read == 0 {
                break;
            }
            output.write_all(&buffer[..read]).context("写入目标文件失败")?;
            if let Some(bar) = &bar {
                bar.inc(read as u64);
            }
        }
        output.sync_all().context("同步目标文件失败")?;
        drop(output);
        let actual = fs::metadata(&temp).map(|meta| meta.len()).unwrap_or(0);
        if actual != total {
            bail!("复制后大小不一致：期望 {total}，实际 {actual}");
        }
        let source_mode = fs::metadata(src)
            .map(|meta| {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o777
            })
            .unwrap_or(0o644);
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(source_mode))?;
        fs::rename(&temp, dest)
            .with_context(|| format!("替换目标文件失败：{}", dest.display()))?;
        Ok(())
    })();

    if let Some(bar) = bar {
        bar.finish_and_clear();
    }
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

struct Observation {
    task: Option<UploadTask>,
    elapsed: Duration,
    verdict: Verdict,
    reason: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Instant,
    Normal,
    Failed,
    Unknown,
}

fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Instant => "秒传",
        Verdict::Normal => "普通上传",
        Verdict::Failed => "上传失败",
        Verdict::Unknown => "无法确认",
    }
}

async fn observe(
    client: &cd2::Client,
    rel: &str,
    file_name: &str,
    size: u64,
    known_keys: &HashSet<String>,
    window: Duration,
) -> Observation {
    let started = Instant::now();
    let deadline = started + window;
    let mut last_task: Option<UploadTask> = None;
    let mut first_seen: Option<Instant> = None;

    loop {
        match client.upload_tasks().await {
            Ok(tasks) => {
                if let Some(task) = select_task(&tasks, rel, file_name, size, known_keys) {
                    if first_seen.is_none() {
                        first_seen = Some(Instant::now());
                    }
                    let status = task.status();
                    if status.is_finished() || status.is_failed() {
                        let elapsed = started.elapsed();
                        let (verdict, reason) =
                            classify_finished(&task, size, elapsed, first_seen);
                        return Observation {
                            task: Some(task.clone()),
                            elapsed,
                            verdict,
                            reason,
                        };
                    }
                    if status == UploadStatus::Transfer
                        && task.transferred_bytes > 0
                        && size > 0
                        && task.transferred_bytes >= size / 100
                    {
                        let elapsed = started.elapsed();
                        return Observation {
                            task: Some(task.clone()),
                            elapsed,
                            verdict: Verdict::Normal,
                            reason: format!(
                                "已实际传输 {}（{}%），不是秒传",
                                format_bytes(task.transferred_bytes),
                                task.transferred_bytes * 100 / size
                            ),
                        };
                    }
                    last_task = Some(task.clone());
                }
            }
            Err(e) => {
                return Observation {
                    task: last_task,
                    elapsed: started.elapsed(),
                    verdict: Verdict::Unknown,
                    reason: format!("读取上传任务失败：{e:#}"),
                };
            }
        }

        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let sleep = (deadline - now).min(Duration::from_millis(500));
        tokio::time::sleep(sleep).await;
    }

    let elapsed = started.elapsed();
    match last_task {
        Some(task) => {
            let status = task.status();
            let reason = match status {
                UploadStatus::Transfer => format!(
                    "观察窗口内仍在传输（{}），判断为普通上传",
                    format_bytes(task.transferred_bytes)
                ),
                UploadStatus::Preprocessing | UploadStatus::WaitForPreprocessing => {
                    "观察窗口内仍在预处理，无法确认（可增大 --window）".to_string()
                }
                UploadStatus::InQueue => {
                    "观察窗口内仍在排队，无法确认（可增大 --window）".to_string()
                }
                _ => format!("任务状态：{}", status.label()),
            };
            let verdict = if status == UploadStatus::Transfer {
                Verdict::Normal
            } else {
                Verdict::Unknown
            };
            Observation {
                task: Some(task),
                elapsed,
                verdict,
                reason,
            }
        }
        None => Observation {
            task: None,
            elapsed,
            verdict: Verdict::Unknown,
            reason: "未在 CD2 上传任务中找到这个文件（可能未开始上传，或项目路径不匹配）"
                .to_string(),
        },
    }
}

fn classify_finished(
    task: &UploadTask,
    size: u64,
    elapsed: Duration,
    first_seen: Option<Instant>,
) -> (Verdict, String) {
    let status = task.status();
    if status == UploadStatus::Skipped {
        return (
            Verdict::Instant,
            "CD2 将任务标记为 Skipped，通常表示云端已有相同文件/秒传".to_string(),
        );
    }
    if status.is_failed() {
        return (
            Verdict::Failed,
            if task.error.is_empty() {
                format!("CD2 状态：{}", status.label())
            } else {
                task.error.clone()
            },
        );
    }

    let metadata_only = size > 0 && task.transferred_bytes < size / 50;
    let seen_for = first_seen
        .map(|seen| seen.elapsed())
        .unwrap_or(Duration::ZERO);
    let fast_large = size >= 32 * 1024 * 1024
        && seen_for <= Duration::from_secs(3)
        && elapsed <= Duration::from_secs(3);
    if metadata_only || fast_large {
        let mut reason = format!(
            "完成时仅传输 {} / {}",
            format_bytes(task.transferred_bytes),
            format_bytes(size)
        );
        if fast_large && !metadata_only {
            reason.push_str("，且大文件在数秒内完成");
        }
        return (Verdict::Instant, reason);
    }
    (
        Verdict::Normal,
        format!(
            "完成，传输 {} / {}",
            format_bytes(task.transferred_bytes),
            format_bytes(size)
        ),
    )
}

fn print_observation(observation: &Observation, size: u64, dest: &Path) {
    println!("\n--- CD2 上传观察 ---");
    match &observation.task {
        Some(task) => {
            let status = task.status();
            let raw = if task.status.is_empty() {
                "-"
            } else {
                task.status.as_str()
            };
            println!("状态：{}（{raw}）", status.label());
            println!(
                "传输：{} / {}",
                format_bytes(task.transferred_bytes),
                format_bytes(size)
            );
            if !task.dest_path.is_empty() {
                println!("远端路径：{}", task.dest_path);
            }
            if !task.error.is_empty() {
                println!("错误：{}", task.error);
            }
        }
        None => println!("状态：未找到对应上传任务"),
    }
    println!("观察耗时：{}", format_duration(observation.elapsed));
    println!("最终文件：{}", dest.display());
    println!(
        "\n结论：{}（{}）",
        verdict_label(observation.verdict),
        observation.reason
    );
}

fn task_matches(task: &UploadTask, rel: &str, file_name: &str, size: u64) -> bool {
    let dest = task.dest_path.replace('\\', "/");
    let rel = rel.replace('\\', "/");
    if size > 0 && task.size > 0 && task.size != size {
        return false;
    }
    dest == rel
        || dest.ends_with(&format!("/{rel}"))
        || dest.ends_with(&format!("/{file_name}"))
        || dest == file_name
}

fn select_task(
    tasks: &[UploadTask],
    rel: &str,
    file_name: &str,
    size: u64,
    known_keys: &HashSet<String>,
) -> Option<UploadTask> {
    let mut candidates: Vec<&UploadTask> = tasks
        .iter()
        .filter(|task| task_matches(task, rel, file_name, size))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // 优先选择本次新出现的任务，避免误用上一次测试留下的同路径任务。
    if let Some(task) = candidates
        .iter()
        .rev()
        .find(|task| !known_keys.contains(&task.key))
    {
        return Some((*task).clone());
    }
    candidates.sort_by_key(|task| {
        (
            task.status().is_finished(),
            task.transferred_bytes,
            task.key.clone(),
        )
    });
    candidates.last().map(|task| (*task).clone())
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs < 1.0 {
        format!("{} ms", duration.as_millis())
    } else if secs < 60.0 {
        format!("{secs:.1} 秒")
    } else {
        let minutes = (secs / 60.0).floor() as u64;
        let rest = secs - minutes as f64 * 60.0;
        format!("{minutes} 分 {rest:.0} 秒")
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Item;

    fn episode(season: i32, episode: i32, size: u64) -> Item {
        Item {
            name: Some(format!("S{season}E{episode}")),
            item_type: Some("Episode".into()),
            parent_index_number: Some(season),
            index_number: Some(episode),
            media_sources: Some(vec![crate::model::MediaSource {
                size: Some(size),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn picks_regular_season_before_specials() {
        let items = vec![
            episode(0, 1, 999),
            episode(2, 1, 100),
            episode(1, 2, 100),
            episode(1, 1, 100),
        ];
        let first = first_episode(items).unwrap();
        assert_eq!(first.season_no(), 1);
        assert_eq!(first.episode_no(), 1);
    }

    #[test]
    fn picks_largest_version_of_first_episode() {
        let items = vec![episode(1, 1, 100), episode(1, 1, 500)];
        let first = first_episode(items).unwrap();
        assert_eq!(first.size(), Some(500));
    }

    #[test]
    fn matches_upload_by_suffix() {
        let task = UploadTask {
            dest_path: "/影视/剧集/测试/S01E01.mkv".into(),
            size: 1024,
            ..Default::default()
        };
        assert!(task_matches(
            &task,
            "影视/剧集/测试/S01E01.mkv",
            "S01E01.mkv",
            1024
        ));
        assert!(!task_matches(
            &task,
            "影视/剧集/测试/S01E01.mkv",
            "S01E01.mkv",
            2048
        ));
    }
}
