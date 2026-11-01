//! 本地审查与临时 → 最终文件夹同步。
//!
//! 审查在本地完成，不访问服务器，也不打开文件读取内容：
//! - 临时文件夹：依据目录项（文件名/路径与大小）统计视频/字幕/元数据/图片与总大小；
//! - 最终文件夹：默认只检查临时文件夹中出现的文件在目标端是否存在、大小是否一致，
//!   不扫描整个目录（网盘/分享目录的 readdir 可能极慢）；`review --full` 时也只读
//!   临时文件所在目录的直属文件，不遍历整棵媒体树，且不读取文件大小。
//! - 按 `SxxExx` 识别剧集，检查集号连续性（缺集）与重复集；
//! - 检查空文件、过小视频与 `.part` 等未完成文件；
//! - 对比临时/最终文件夹，列出待复制、待覆盖等差异；`--full` 时额外列出仅最终有的文件。
//!
//! 同步默认只复制缺失文件和大小不一致的文件；`.part`、异常文件会被跳过，
//! 最终文件夹里多出来的文件不会被删除。同步只按待复制文件逐个检查最终路径，
//! 不递归扫描最终文件夹——网盘挂载上的无关目录（如分享目录）可能极慢。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};

use crate::config;
use crate::util::{self, format_bytes, verbose};

const MAX_DEPTH: usize = 32;
const LIST_LIMIT: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Video,
    Subtitle,
    Metadata,
    Artwork,
    Incomplete,
    Other,
}

#[derive(Clone)]
struct FileEntry {
    rel: PathBuf,
    /// `None` 表示只扫描了目录结构，没有读取文件大小。
    size: Option<u64>,
    kind: Kind,
    error: Option<String>,
    series: Option<String>,
    season: Option<i32>,
    episode: Option<i32>,
}

#[derive(Default)]
struct SeasonStats {
    /// 集号 → 文件数（多版本会大于 1）。
    counts: HashMap<i32, usize>,
}

struct FolderReport {
    root: PathBuf,
    exists: bool,
    files: Vec<FileEntry>,
    scan_errors: Vec<String>,
    /// `None` 表示没有读取文件大小（只扫描了目录结构）。
    total_size: Option<u64>,
    videos: usize,
    episodes: usize,
    movies: usize,
    subtitles: usize,
    metadata: usize,
    artwork: usize,
    incomplete: usize,
    incomplete_tasks: usize,
    other: usize,
    series_seasons: BTreeMap<(String, i32), SeasonStats>,
    movie_names: BTreeSet<String>,
    incomplete_files: BTreeMap<PathBuf, Option<u64>>,
}

impl FolderReport {
    fn empty(root: &Path, exists: bool) -> Self {
        Self {
            root: root.to_path_buf(),
            exists,
            files: Vec::new(),
            scan_errors: Vec::new(),
            total_size: None,
            videos: 0,
            episodes: 0,
            movies: 0,
            subtitles: 0,
            metadata: 0,
            artwork: 0,
            incomplete: 0,
            incomplete_tasks: 0,
            other: 0,
            series_seasons: BTreeMap::new(),
            movie_names: BTreeSet::new(),
            incomplete_files: BTreeMap::new(),
        }
    }

    fn errors(&self) -> impl Iterator<Item = (&Path, &str)> {
        self.files
            .iter()
            .filter_map(|f| f.error.as_deref().map(|e| (f.rel.as_path(), e)))
    }
}

#[derive(Clone)]
struct PlanFile {
    rel: PathBuf,
    /// `None` 表示扫描时未读取大小。
    size: Option<u64>,
}

/// 差异列表的一行：同一季的剧集合并成一行，其它文件逐条显示。
struct DisplayRow {
    /// `(剧名, 季)`；`None` 表示电影/字幕等非剧集文件。
    group: Option<(String, i32)>,
    files: Vec<PlanFile>,
}

/// 按「剧名 + 季」聚合差异文件，保持首次出现顺序。
fn group_plan_files(files: &[PlanFile]) -> Vec<DisplayRow> {
    let mut rows: Vec<DisplayRow> = Vec::new();
    let mut index: HashMap<(String, i32), usize> = HashMap::new();
    for file in files {
        let key = match parse_episode(&file.rel) {
            (Some(series), Some(season), _) => Some((series, season)),
            _ => None,
        };
        if let Some(key) = key {
            if let Some(&row) = index.get(&key) {
                rows[row].files.push(file.clone());
            } else {
                index.insert(key.clone(), rows.len());
                rows.push(DisplayRow {
                    group: Some(key),
                    files: vec![file.clone()],
                });
            }
        } else {
            rows.push(DisplayRow {
                group: None,
                files: vec![file.clone()],
            });
        }
    }
    // 目录遍历顺序不保证稳定；按相对路径排序，输出可重复。
    rows.sort_by(|a, b| a.files[0].rel.cmp(&b.files[0].rel));
    rows
}

/// 汇总一组文件的大小；有任意文件未读取大小时返回 `None`。
fn sum_sizes(files: &[PlanFile]) -> Option<u64> {
    files
        .iter()
        .try_fold(0u64, |total, file| Some(total + file.size?))
}

/// `（1.2 GiB）` 或空串（未读取大小时不显示）。
fn size_note(size: Option<u64>) -> String {
    match size {
        Some(size) => format!("（{}）", format_bytes(size)),
        None => String::new(),
    }
}

/// `，1.2 GiB` 或空串（大小未知时不显示总量）。
fn total_note(files: &[PlanFile]) -> String {
    match sum_sizes(files) {
        Some(bytes) => format!("，{}", format_bytes(bytes)),
        None => String::new(),
    }
}

/// 渲染差异列表；`-v` 时展开为逐个文件。
fn render_plan_rows(rows: &[DisplayRow], verbose: bool) -> Vec<String> {
    if verbose {
        return rows
            .iter()
            .flat_map(|row| row.files.iter())
            .map(|file| format!("{}{}", file.rel.display(), size_note(file.size)))
            .collect();
    }
    rows.iter()
        .map(|row| match &row.group {
            Some((series, season)) if row.files.len() > 1 => {
                format!(
                    "{series} S{season:02}（{} 集{}）",
                    row.files.len(),
                    total_note(&row.files)
                )
            }
            Some((series, season)) => {
                let file = &row.files[0];
                let episode = parse_episode(&file.rel).2.unwrap_or(0);
                format!(
                    "{series} S{season:02}E{episode:02}{}",
                    size_note(file.size)
                )
            }
            None => {
                let file = &row.files[0];
                format!("{}{}", file.rel.display(), size_note(file.size))
            }
        })
        .collect()
}

struct SyncPlan {
    to_copy: Vec<PlanFile>,
    overwrite: Vec<PlanFile>,
    same: usize,
    /// 仅 `review --full` 全量扫描时可得；定向比对不统计。
    final_only: Vec<PlanFile>,
    skipped: Vec<(PathBuf, String)>,
    copy_bytes: u64,
}

impl SyncPlan {
    fn new() -> Self {
        Self {
            to_copy: Vec::new(),
            overwrite: Vec::new(),
            same: 0,
            final_only: Vec::new(),
            skipped: Vec::new(),
            copy_bytes: 0,
        }
    }
}

/// `embydown review`：审查临时文件夹，并与最终文件夹对比。
///
/// 默认只检查临时文件夹里的文件在最终文件夹中的对应路径，不递归扫描最终文件夹——
/// 网盘/分享目录上一次 readdir 可能要等很久。`full` 为 true 时才完整扫描。
pub fn run(temp: Option<PathBuf>, final_dir: Option<PathBuf>, full: bool) -> Result<()> {
    let (temp, final_dir) = resolve_dirs(temp, final_dir)?;
    let temp_report = scan(&temp);

    print_title("本地审查");
    print_folder(&temp_report, "临时文件夹");

    let Some(final_dir) = final_dir else {
        println!("\n[最终文件夹] 未设置（用 --final 指定，或在交互模式「下载设置」中设置）");
        return Ok(());
    };

    let plan = if full {
        // 只读临时文件所在的那几个目录的直属文件，不遍历整棵媒体树。
        let dirs = parent_scopes(&temp_report);
        let note = if dirs.is_empty() {
            "临时文件夹为空，未扫描最终文件夹".to_string()
        } else {
            format!(
                "只扫描临时文件所在的 {} 个目录（不遍历整棵媒体树；只读结构，不比较大小）",
                dirs.len()
            )
        };
        print_folder_heading(&final_dir, "最终文件夹", Some(&note));
        let final_report = scan_structure(&final_dir, Some(&dirs));
        print_folder_body(&final_report);
        build_plan(&temp_report, &final_report, false)
    } else {
        println!("\n[最终文件夹] {}", final_dir.display());
        if !final_dir.exists() {
            println!("  不存在");
        } else {
            println!("  只检查临时文件夹中的文件是否存在（不扫描整个目录，完整扫描用 review --full）");
        }
        build_plan_targeted(&temp_report, &final_dir, false)
    };

    println!("\n[临时 → 最终 差异]");
    print_plan(&plan, true);
    Ok(())
}

/// `embydown sync`：把临时文件夹复制到最终文件夹。
pub fn sync(
    temp: Option<PathBuf>,
    final_dir: Option<PathBuf>,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    let (temp, final_dir) = resolve_dirs(temp, final_dir)?;
    let final_dir = final_dir
        .context("未设置最终文件夹：请用 --final <DIR> 指定，或在交互模式「下载设置」中设置")?;
    if !temp.exists() {
        bail!("临时文件夹不存在：{}", temp.display());
    }
    ensure_separate(&temp, &final_dir)?;

    let temp_report = scan(&temp);
    // 只 stat 待同步文件在最终文件夹中的对应路径；不递归扫描最终文件夹，
    // 否则网盘挂载上的无关大目录/分享目录会让同步看起来像卡死。
    let plan = build_plan_targeted(&temp_report, &final_dir, force);

    print_title(if dry_run {
        "同步预览"
    } else {
        "同步到最终文件夹"
    });
    println!("临时：{}", temp.display());
    println!("最终：{}", final_dir.display());
    print_plan(&plan, dry_run);

    if dry_run {
        println!("\n（预览模式，未复制任何文件）");
        return Ok(());
    }
    if plan.to_copy.is_empty() && plan.overwrite.is_empty() {
        println!("\n✅ 两个文件夹已经一致，无需同步");
        return Ok(());
    }

    let files: Vec<&PlanFile> = plan.to_copy.iter().chain(plan.overwrite.iter()).collect();
    let bar = progress_bar(plan.copy_bytes);
    let mut copied = 0usize;
    let mut copied_bytes = 0u64;
    let mut failures: Vec<(&PlanFile, anyhow::Error)> = Vec::new();
    for file in &files {
        if let Some(bar) = &bar {
            bar.set_message(file.rel.display().to_string());
        }
        // 临时文件夹是本地目录，扫描时总能拿到大小。
        let expected_size = file.size.unwrap_or(0);
        match copy_atomic(
            &temp.join(&file.rel),
            &final_dir.join(&file.rel),
            expected_size,
            bar.as_ref(),
        ) {
            Ok(()) => {
                copied += 1;
                copied_bytes += expected_size;
            }
            Err(e) => failures.push((file, e)),
        }
    }

    // 网盘/FUSE 挂载的临时错误（超时、ENOTRECOVERABLE 等）往往几十秒内恢复。
    // 主流程结束后自动重试一次，避免用户手动重跑整个同步。
    if !failures.is_empty() {
        let (retry, permanent): (Vec<_>, Vec<_>) = failures
            .into_iter()
            .partition(|(_, error)| is_transient_error(error));
        if !retry.is_empty() {
            println!(
                "\n! {} 个文件因网盘/云端临时错误失败，5 秒后自动重试…",
                retry.len()
            );
            std::thread::sleep(Duration::from_secs(5));
            if let Some(bar) = &bar {
                bar.set_position(copied_bytes);
            }
            let mut still_failed = Vec::new();
            for (file, _) in retry {
                if let Some(bar) = &bar {
                    bar.set_message(format!("重试 {}", file.rel.display()));
                }
                let expected_size = file.size.unwrap_or(0);
                match copy_atomic(
                    &temp.join(&file.rel),
                    &final_dir.join(&file.rel),
                    expected_size,
                    bar.as_ref(),
                ) {
                    Ok(()) => {
                        copied += 1;
                        copied_bytes += expected_size;
                    }
                    Err(e) => still_failed.push((file, e)),
                }
            }
            failures = permanent;
            failures.extend(still_failed);
        } else {
            failures = permanent;
        }
    }

    if let Some(bar) = bar {
        bar.finish_and_clear();
    }

    println!(
        "\n✅ 已复制 {copied} 个文件，共 {}",
        format_bytes(copied_bytes)
    );
    if !failures.is_empty() {
        println!("✗ 失败 {} 个：", failures.len());
        for (file, error) in &failures {
            println!(
                "   - {}: {}",
                file.rel.display(),
                crate::emby::redact_text(&format!("{error:#}"))
            );
        }
        if failures.iter().any(|(_, error)| is_transient_error(error)) {
            println!(
                "\n  ! 网盘/云端暂时不可用。挂载恢复后重新运行 `embydown sync` 即可，只会复制缺失文件。"
            );
        }
        bail!("{} 个文件同步失败", failures.len());
    }
    Ok(())
}

/// 网盘挂载/CDN 常见的临时错误：稍后重试即可恢复。
fn is_transient_io(error: &std::io::Error) -> bool {
    // EINTR / EIO / EAGAIN / EBUSY / ETIMEDOUT / EREMOTEIO / ENOTRECOVERABLE
    matches!(
        error.raw_os_error(),
        Some(4 | 5 | 11 | 16 | 110 | 121 | 131)
    ) || matches!(
        error.kind(),
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
    )
}

fn is_transient_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<std::io::Error>().is_some_and(is_transient_io))
}

fn resolve_dirs(
    temp: Option<PathBuf>,
    final_dir: Option<PathBuf>,
) -> Result<(PathBuf, Option<PathBuf>)> {
    let cfg = config::load_optional()?.unwrap_or_default();
    let temp = match temp {
        Some(path) => path,
        None => {
            let configured = cfg.settings.temp_dir.trim();
            PathBuf::from(if configured.is_empty() {
                "./downloads"
            } else {
                configured
            })
        }
    };
    let temp = util::expand_tilde(&temp)?;
    let final_dir = match final_dir {
        Some(path) => Some(util::expand_tilde(&path)?),
        None => {
            let configured = cfg.settings.final_dir.trim();
            if configured.is_empty() {
                None
            } else {
                Some(util::expand_tilde(Path::new(configured))?)
            }
        }
    };
    Ok((temp, final_dir))
}

/// 两个目录不能相同，也不能互相包含，否则同步会递归复制自己。
pub(crate) fn ensure_separate(temp: &Path, final_dir: &Path) -> Result<()> {
    let temp_abs = fs::canonicalize(temp).unwrap_or_else(|_| temp.to_path_buf());
    let final_abs = if final_dir.exists() {
        fs::canonicalize(final_dir).unwrap_or_else(|_| final_dir.to_path_buf())
    } else {
        std::path::absolute(final_dir).unwrap_or_else(|_| final_dir.to_path_buf())
    };
    if temp_abs == final_abs {
        bail!("临时文件夹和最终文件夹不能是同一个目录");
    }
    if final_abs.starts_with(&temp_abs) || temp_abs.starts_with(&final_abs) {
        bail!("临时文件夹和最终文件夹不能互相包含");
    }
    Ok(())
}

/// 临时文件所在的目录（相对路径）；最终端只读这些目录的直属文件。
fn parent_scopes(temp: &FolderReport) -> BTreeSet<PathBuf> {
    temp.files
        .iter()
        .filter_map(|file| file.rel.parent())
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect()
}

/// 扫描文件夹并读取文件大小（用于本地临时文件夹）。
fn scan(root: &Path) -> FolderReport {
    scan_with(root, true, None)
}

/// 只扫描目录结构，不读取文件大小；`dirs` 为 `Some` 时只读这些子目录的直属文件。
fn scan_structure(root: &Path, dirs: Option<&BTreeSet<PathBuf>>) -> FolderReport {
    scan_with(root, false, dirs)
}

fn scan_with(
    root: &Path,
    fetch_sizes: bool,
    dirs: Option<&BTreeSet<PathBuf>>,
) -> FolderReport {
    if !root.exists() {
        return FolderReport::empty(root, false);
    }
    if !root.is_dir() {
        let mut report = FolderReport::empty(root, true);
        report.scan_errors.push("路径存在但不是文件夹".to_string());
        return report;
    }
    let mut files = Vec::new();
    let mut scan_errors = Vec::new();
    match dirs {
        None => walk(root, root, &mut files, 0, &mut scan_errors, fetch_sizes, true),
        Some(dirs) => {
            // 只读临时文件所在目录的直属文件，不递归、不碰其它目录。
            for dir in dirs {
                let path = root.join(dir);
                match fs::symlink_metadata(&path) {
                    Ok(meta) if meta.file_type().is_dir() => {
                        walk(root, &path, &mut files, 1, &mut scan_errors, fetch_sizes, false);
                    }
                    // 目标目录不存在：差异里会显示为待复制。
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => scan_errors.push(format!("无法读取 {}：{e}", path.display())),
                }
            }
        }
    }
    summarize(root, files, scan_errors)
}

fn walk(
    root: &Path,
    dir: &Path,
    out: &mut Vec<FileEntry>,
    depth: usize,
    errors: &mut Vec<String>,
    fetch_sizes: bool,
    recurse: bool,
) {
    if depth > MAX_DEPTH {
        errors.push(format!("目录层级过深，已跳过：{}", dir.display()));
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            errors.push(format!("无法读取 {}：{e}", dir.display()));
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "@eaDir" || name == "#recycle" {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(e) => {
                errors.push(format!("无法读取 {}：{e}", path.display()));
                continue;
            }
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if recurse {
                walk(root, &path, out, depth + 1, errors, fetch_sizes, recurse);
            }
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        // 网盘挂载上逐文件 stat 可能极慢；不需要大小时只按目录项归类。
        let size = if fetch_sizes {
            entry.metadata().ok().map(|meta| meta.len())
        } else {
            None
        };
        let (kind, error) = classify_and_check(rel, size);
        let (series, season, episode) = if kind == Kind::Video {
            parse_episode(rel)
        } else {
            (None, None, None)
        };
        out.push(FileEntry {
            rel: rel.to_path_buf(),
            size,
            kind,
            error,
            series,
            season,
            episode,
        });
    }
}

/// 仅依据目录项判断：文件名决定类型，大小来自目录项，不打开文件。
fn classify_and_check(rel: &Path, size: Option<u64>) -> (Kind, Option<String>) {
    let name = rel
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let kind = classify(&name);
    if kind == Kind::Incomplete {
        return (kind, None);
    }
    match size {
        Some(0) => (kind, Some("空文件（0 字节）".into())),
        Some(size) if kind == Kind::Video && size < 1024 => {
            (kind, Some(format!("文件过小（{}）", format_bytes(size))))
        }
        _ => (kind, None),
    }
}

fn classify(name: &str) -> Kind {
    if name.ends_with(".part")
        || name.ends_with(".part.json")
        || name.ends_with(".tmp")
        || name.ends_with(".download")
        || name.ends_with(".aria2")
        || name.ends_with(".!qb")
    {
        return Kind::Incomplete;
    }
    let ext = name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    match ext {
        "mkv" | "mp4" | "m4v" | "avi" | "ts" | "m2ts" | "mts" | "mov" | "wmv" | "flv" | "webm"
        | "mpg" | "mpeg" | "vob" | "rmvb" | "rm" | "ogv" | "3gp" | "divx" | "xvid" => Kind::Video,
        "srt" | "ass" | "ssa" | "sub" | "vtt" | "sup" | "idx" => Kind::Subtitle,
        "nfo" | "xml" => Kind::Metadata,
        "jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif" => Kind::Artwork,
        _ => Kind::Other,
    }
}

fn parse_episode(rel: &Path) -> (Option<String>, Option<i32>, Option<i32>) {
    let name = rel
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let Some((season, episode)) = crate::model::parse_season_episode(&name) else {
        return (None, None, None);
    };
    (series_from_rel(rel), Some(season), Some(episode))
}

fn series_from_rel(rel: &Path) -> Option<String> {
    let components: Vec<String> = rel
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    for (i, component) in components.iter().enumerate().rev() {
        if component.to_ascii_lowercase().starts_with("season") && i > 0 {
            let name = clean_series_dir(&components[i - 1]);
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    let stem = rel.file_stem()?.to_string_lossy();
    series_from_name(&stem)
}

fn clean_series_dir(dir: &str) -> String {
    let mut name = dir.trim();
    if name.ends_with(']')
        && let Some(pos) = name.rfind('[')
    {
        name = name[..pos].trim_end();
    }
    if name.ends_with(')')
        && let Some(pos) = name.rfind('(')
    {
        let inner = &name[pos + 1..name.len() - 1];
        if inner.len() == 4 && inner.chars().all(|c| c.is_ascii_digit()) {
            name = name[..pos].trim_end();
        }
    }
    name.to_string()
}

fn series_from_name(stem: &str) -> Option<String> {
    let lower = stem.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b's' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                let mut k = j;
                while k < bytes.len() && matches!(bytes[k], b' ' | b'.' | b'_' | b'-') {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == b'e' {
                    let mut m = k + 1;
                    while m < bytes.len() && bytes[m].is_ascii_digit() {
                        m += 1;
                    }
                    if m > k + 1 {
                        let name = stem[..i].trim_matches(|c: char| {
                            c.is_whitespace() || matches!(c, '-' | '_' | '.' | '·')
                        });
                        if !name.is_empty() {
                            return Some(name.to_string());
                        }
                    }
                }
            }
        }
        i += 1;
    }
    None
}

fn summarize(root: &Path, files: Vec<FileEntry>, scan_errors: Vec<String>) -> FolderReport {
    let mut report = FolderReport::empty(root, true);
    report.scan_errors = scan_errors;
    report.total_size = files
        .iter()
        .try_fold(0u64, |total, file| Some(total + file.size?));
    for file in &files {
        match file.kind {
            Kind::Video => {
                report.videos += 1;
                if let (Some(series), Some(season), Some(episode)) =
                    (&file.series, file.season, file.episode)
                {
                    if season >= 0 && episode >= 0 {
                        report.episodes += 1;
                        let stats = report
                            .series_seasons
                            .entry((series.clone(), season))
                            .or_default();
                        *stats.counts.entry(episode).or_insert(0) += 1;
                    }
                } else {
                    report.movies += 1;
                    if let Some(name) = display_video_name(&file.rel) {
                        report.movie_names.insert(name);
                    }
                }
            }
            Kind::Subtitle => report.subtitles += 1,
            Kind::Metadata => report.metadata += 1,
            Kind::Artwork => report.artwork += 1,
            Kind::Incomplete => {
                report.incomplete += 1;
                // `.part` 与 `.part.json` 属于同一个任务，合并成一条。
                let target = incomplete_target(&file.rel);
                let size = report.incomplete_files.entry(target).or_insert(None);
                *size = (*size).max(file.size);
            }
            Kind::Other => report.other += 1,
        }
    }
    report.incomplete_tasks = report.incomplete_files.len();
    report.files = files;
    report
}

/// `.part` / `.part.json` → 完成后的目标文件；其它未完成文件保持原路径。
fn incomplete_target(rel: &Path) -> PathBuf {
    let name = rel
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    for suffix in [".part.json", ".part", ".tmp", ".download", ".aria2", ".!qb"] {
        if let Some(base) = name.strip_suffix(suffix) {
            return rel.with_file_name(base);
        }
    }
    rel.to_path_buf()
}

fn display_video_name(rel: &Path) -> Option<String> {
    let stem = rel.file_stem()?.to_string_lossy().trim().to_string();
    (!stem.is_empty()).then_some(stem)
}

fn print_title(title: &str) {
    println!();
    println!("{}", "─".repeat(60));
    println!(" {title}");
    println!("{}", "─".repeat(60));
}

fn print_folder(report: &FolderReport, label: &str) {
    print_folder_heading(&report.root, label, None);
    print_folder_body(report);
}

fn print_folder_heading(root: &Path, label: &str, note: Option<&str>) {
    println!("\n[{label}] {}", root.display());
    if let Some(note) = note {
        println!("  {note}");
    }
}

fn print_folder_body(report: &FolderReport) {
    if !report.exists {
        println!("  不存在");
        for error in &report.scan_errors {
            println!("  ! {error}");
        }
        return;
    }
    let size_text = match report.total_size {
        Some(bytes) => format!("，共 {}", format_bytes(bytes)),
        None => String::new(),
    };
    println!(
        "  文件：{} 个{}（视频 {} · 字幕 {} · 元数据 {} · 图片 {} · 其他 {} · 未完成 {}）",
        report.files.len(),
        size_text,
        report.videos,
        report.subtitles,
        report.metadata,
        report.artwork,
        report.other,
        report.incomplete,
    );
    for error in &report.scan_errors {
        println!("  ! {error}");
    }
    if report.files.is_empty() {
        println!("  （空文件夹）");
        return;
    }

    if !report.series_seasons.is_empty() {
        let mut series: BTreeMap<&str, BTreeSet<i32>> = BTreeMap::new();
        for (name, season) in report.series_seasons.keys() {
            series.entry(name.as_str()).or_default().insert(*season);
        }
        println!(
            "  剧集：{} 部 / {} 季 / {} 集",
            series.len(),
            report.series_seasons.len(),
            report.episodes
        );
        let items: Vec<String> = series
            .iter()
            .map(|(name, seasons)| format!("{name}（{} 季）", seasons.len()))
            .collect();
        print_items(&items, "- ");
    }
    if !report.movie_names.is_empty() {
        println!("  影片：{} 部", report.movie_names.len());
        let items: Vec<String> = report.movie_names.iter().cloned().collect();
        print_items(&items, "- ");
    }

    let mut missing_lines = Vec::new();
    for ((series, season), stats) in &report.series_seasons {
        if *season <= 0 {
            continue;
        }
        let min = stats.counts.keys().copied().min().unwrap_or(0);
        let max = stats.counts.keys().copied().max().unwrap_or(0);
        let mut missing: Vec<i32> = (min..=max)
            .filter(|episode| !stats.counts.contains_key(episode))
            .collect();
        // 如果上一季的最后一集正好是本站第一集 - 1，说明这部剧用连续编号
        // （S02E25 这种），不能把 E01..E24 误报成缺集。
        if min > 1 && !connected_to_previous_season(report, series, *season, min) {
            missing.extend(1..min);
            missing.sort_unstable();
        }
        if !missing.is_empty() {
            missing_lines.push(format!(
                "{series} S{season:02}：缺少 {}（已有 {} 集）",
                format_episodes(&missing),
                stats.counts.len()
            ));
        }
    }
    if !missing_lines.is_empty() {
        println!("  缺集：");
        print_items(&missing_lines, "- ");
    }

    let mut duplicate_lines = Vec::new();
    for ((series, season), stats) in &report.series_seasons {
        let duplicates: Vec<i32> = stats
            .counts
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(episode, _)| *episode)
            .collect();
        if !duplicates.is_empty() {
            duplicate_lines.push(format!(
                "{series} S{season:02}：{} 有多个文件（可能是多版本）",
                format_episodes(&duplicates)
            ));
        }
    }
    if !duplicate_lines.is_empty() {
        println!("  重复集：");
        print_items(&duplicate_lines, "- ");
    }

    if !report.incomplete_files.is_empty() {
        if report.incomplete_tasks == report.incomplete {
            println!("  未完成：{} 个", report.incomplete_tasks);
        } else {
            println!(
                "  未完成：{} 个文件 / {} 个任务",
                report.incomplete, report.incomplete_tasks
            );
        }
        let items: Vec<String> = report
            .incomplete_files
            .iter()
            .map(|(path, size)| match size {
                Some(bytes) => format!("{}（{}）", path.display(), format_bytes(*bytes)),
                None => path.display().to_string(),
            })
            .collect();
        print_items(&items, "- ");
    }

    let errors: Vec<String> = report
        .errors()
        .map(|(path, error)| format!("{}：{error}", path.display()))
        .collect();
    if !errors.is_empty() {
        println!("  异常：{} 个", errors.len());
        print_items(&errors, "- ");
    }
}

fn connected_to_previous_season(
    report: &FolderReport,
    series: &str,
    season: i32,
    first_episode: i32,
) -> bool {
    report.series_seasons.iter().any(|((name, other), stats)| {
        name == series
            && *other == season - 1
            && stats.counts.keys().copied().max() == Some(first_episode - 1)
    })
}

fn format_episodes(episodes: &[i32]) -> String {
    const SHOWN: usize = 10;
    let list: Vec<String> = episodes
        .iter()
        .take(SHOWN)
        .map(|episode| format!("E{episode:02}"))
        .collect();
    if episodes.len() > SHOWN {
        format!("{} 等 {} 集", list.join("、"), episodes.len())
    } else {
        list.join("、")
    }
}

fn print_items(items: &[String], prefix: &str) {
    let limit = if verbose() >= 1 {
        items.len()
    } else {
        LIST_LIMIT
    };
    for item in items.iter().take(limit) {
        println!("    {prefix}{item}");
    }
    if items.len() > limit {
        println!(
            "    … 还有 {} 项（加 -v 展开全部文件）",
            items.len() - limit
        );
    }
}

fn build_plan(temp: &FolderReport, final_report: &FolderReport, force: bool) -> SyncPlan {
    let final_map: HashMap<&Path, &FileEntry> = final_report
        .files
        .iter()
        .filter(|file| file.kind != Kind::Incomplete)
        .map(|file| (file.rel.as_path(), file))
        .collect();
    let mut plan = SyncPlan::new();

    for file in &temp.files {
        if skip_temp_file(file, &mut plan) {
            continue;
        }
        match final_map.get(file.rel.as_path()) {
            None => {
                plan.copy_bytes += file.size.unwrap_or(0);
                plan.to_copy.push(PlanFile {
                    rel: file.rel.clone(),
                    size: file.size,
                });
            }
            Some(existing) => {
                // 结构扫描不读取最终文件夹的大小；此时只按“是否存在”判断。
                let differs = match (file.size, existing.size) {
                    (Some(temp), Some(final_size)) => temp != final_size,
                    _ => false,
                };
                if force || differs {
                    plan.copy_bytes += file.size.unwrap_or(0);
                    plan.overwrite.push(PlanFile {
                        rel: file.rel.clone(),
                        size: file.size,
                    });
                } else {
                    plan.same += 1;
                }
            }
        }
    }

    let temp_files: HashSet<&Path> = temp.files.iter().map(|file| file.rel.as_path()).collect();
    for file in &final_report.files {
        if file.kind == Kind::Incomplete {
            continue;
        }
        if !temp_files.contains(file.rel.as_path()) {
            plan.final_only.push(PlanFile {
                rel: file.rel.clone(),
                size: file.size,
            });
        }
    }
    plan
}

/// 定向比对：只检查临时文件夹里的文件在最终文件夹中的对应路径。
///
/// 不递归扫描最终文件夹，因此不会碰到与本次同步无关的目录（例如网盘上的
/// 分享目录）；代价是不再统计「仅最终文件夹有」的文件——同步本来也不会删除它们。
fn build_plan_targeted(temp: &FolderReport, final_dir: &Path, force: bool) -> SyncPlan {
    let mut plan = SyncPlan::new();
    for file in &temp.files {
        if skip_temp_file(file, &mut plan) {
            continue;
        }
        let existing = fs::symlink_metadata(final_dir.join(&file.rel))
            .ok()
            .filter(|meta| meta.file_type().is_file())
            .map(|meta| meta.len());
        match existing {
            None => {
                plan.copy_bytes += file.size.unwrap_or(0);
                plan.to_copy.push(PlanFile {
                    rel: file.rel.clone(),
                    size: file.size,
                });
            }
            Some(size) if force || file.size.is_none_or(|temp| temp != size) => {
                plan.copy_bytes += file.size.unwrap_or(0);
                plan.overwrite.push(PlanFile {
                    rel: file.rel.clone(),
                    size: file.size,
                });
            }
            Some(_) => plan.same += 1,
        }
    }
    plan
}

/// 未完成或异常文件不参与复制；已记录到 `skipped` 时返回 true。
fn skip_temp_file(file: &FileEntry, plan: &mut SyncPlan) -> bool {
    if file.kind == Kind::Incomplete {
        let target = incomplete_target(&file.rel);
        if !plan.skipped.iter().any(|(path, _)| path == &target) {
            plan.skipped.push((target, "未完成".into()));
        }
        return true;
    }
    if let Some(error) = &file.error {
        plan.skipped.push((file.rel.clone(), error.clone()));
        return true;
    }
    false
}

fn print_plan(plan: &SyncPlan, preview: bool) {
    if plan.to_copy.is_empty() && plan.overwrite.is_empty() {
        println!("  待复制：0 个");
    } else {
        println!(
            "  待复制：{} 个{}",
            plan.to_copy.len(),
            total_note(&plan.to_copy)
        );
        let items =
            render_plan_rows(&group_plan_files(&plan.to_copy), verbose() >= 1);
        print_items(&items, "- ");
        if !plan.overwrite.is_empty() {
            println!(
                "  待覆盖：{} 个{}（最终文件夹里已有但大小不同）",
                plan.overwrite.len(),
                total_note(&plan.overwrite)
            );
            let items =
                render_plan_rows(&group_plan_files(&plan.overwrite), verbose() >= 1);
            print_items(&items, "- ");
        }
    }
    println!("  已一致：{} 个", plan.same);
    if !plan.final_only.is_empty() {
        println!(
            "  仅最终文件夹有：{} 个{}（同步不会删除）",
            plan.final_only.len(),
            total_note(&plan.final_only)
        );
        let items =
            render_plan_rows(&group_plan_files(&plan.final_only), verbose() >= 1);
        print_items(&items, "- ");
    }
    if !plan.skipped.is_empty() {
        println!(
            "  跳过：{} 个（未完成或异常，修复后重新同步）",
            plan.skipped.len()
        );
        let items: Vec<String> = plan
            .skipped
            .iter()
            .map(|(path, reason)| format!("{}：{reason}", path.display()))
            .collect();
        print_items(&items, "- ");
    }
    if preview && (plan.to_copy.is_empty() && plan.overwrite.is_empty()) {
        println!("  （没有需要同步的文件）");
    }
}

fn progress_bar(total: u64) -> Option<ProgressBar> {
    if total == 0 {
        return None;
    }
    let bar = ProgressBar::new(total);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} {bytes}/{total_bytes} {bar:32.cyan/blue} {percent}%  {msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar()),
    );
    Some(bar)
}

/// 创建目标目录；FUSE/网盘挂载偶尔会返回可恢复的临时错误，短退避后重试。
fn create_parent_dirs(parent: &Path) -> Result<()> {
    let mut delay = Duration::from_millis(500);
    for attempt in 0..3 {
        match fs::create_dir_all(parent) {
            Ok(()) => return Ok(()),
            Err(error) if attempt < 2 && is_transient_io(&error) => {
                std::thread::sleep(delay);
                delay *= 2;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("创建目录失败：{}", parent.display()));
            }
        }
    }
    unreachable!("重试循环最多执行三次")
}

fn copy_atomic(
    src: &Path,
    dest: &Path,
    expected_size: u64,
    bar: Option<&ProgressBar>,
) -> Result<()> {
    let parent = dest
        .parent()
        .with_context(|| format!("无法确定目标目录：{}", dest.display()))?;
    create_parent_dirs(parent)?;
    let name = dest
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let temp = parent.join(format!(".{name}.embysync-{}", std::process::id()));

    let result = (|| -> Result<()> {
        let mut input =
            File::open(src).with_context(|| format!("打开源文件失败：{}", src.display()))?;
        let source_mode = fs::metadata(src)
            .map(|meta| {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o777
            })
            .unwrap_or(0o644);
        let mut output = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
            .with_context(|| format!("创建临时文件失败：{}", temp.display()))?;
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            let read = input.read(&mut buffer).context("读取源文件失败")?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .context("写入目标文件失败")?;
            if let Some(bar) = bar {
                bar.inc(read as u64);
            }
        }
        output.sync_all().context("同步目标文件失败")?;
        drop(output);
        let actual = fs::metadata(&temp)
            .map(|meta| meta.len())
            .unwrap_or_default();
        if actual != expected_size {
            bail!("复制后大小不一致：期望 {expected_size}，实际 {actual}");
        }
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(source_mode))
            .with_context(|| format!("设置权限失败：{}", temp.display()))?;
        fs::rename(&temp, dest).with_context(|| format!("替换目标文件失败：{}", dest.display()))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_incomplete_before_extension() {
        assert_eq!(classify("movie.mkv.part"), Kind::Incomplete);
        assert_eq!(classify("movie.mkv.part.json"), Kind::Incomplete);
        assert_eq!(classify("movie.mkv"), Kind::Video);
        assert_eq!(classify("movie.zh.srt"), Kind::Subtitle);
        assert_eq!(classify("movie.nfo"), Kind::Metadata);
        assert_eq!(classify("poster.jpg"), Kind::Artwork);
    }

    #[test]
    fn detects_missing_episode_numbers() {
        let mut stats = SeasonStats::default();
        for episode in [1, 2, 4, 4] {
            *stats.counts.entry(episode).or_insert(0) += 1;
        }
        let max = stats.counts.keys().copied().max().unwrap();
        let missing: Vec<i32> = (1..=max)
            .filter(|episode| !stats.counts.contains_key(episode))
            .collect();
        assert_eq!(missing, vec![3]);
        assert_eq!(stats.counts[&4], 2);
    }

    #[test]
    fn detects_continuous_season_numbering() {
        let mut report = FolderReport::empty(Path::new("/"), true);
        let mut stats = SeasonStats::default();
        stats.counts.insert(24, 1);
        report.series_seasons.insert(("剧".into(), 1), stats);
        assert!(connected_to_previous_season(&report, "剧", 2, 25));
        assert!(!connected_to_previous_season(&report, "剧", 2, 24));
    }

    #[test]
    fn extracts_series_from_default_path() {
        let rel = Path::new("影视/剧集/庆余年 (2019)/Season 00001/庆余年 - S00001E00002.mkv");
        assert_eq!(series_from_rel(rel).as_deref(), Some("庆余年"));
        let (series, season, episode) = parse_episode(rel);
        assert_eq!(series.as_deref(), Some("庆余年"));
        assert_eq!(season, Some(1));
        assert_eq!(episode, Some(2));
    }

    #[test]
    fn groups_part_and_part_json() {
        assert_eq!(
            incomplete_target(Path::new("show/S01E01.mkv.part")),
            PathBuf::from("show/S01E01.mkv")
        );
        assert_eq!(
            incomplete_target(Path::new("show/S01E01.mkv.part.json")),
            PathBuf::from("show/S01E01.mkv")
        );
    }

    #[test]
    fn flags_empty_and_tiny_videos_without_opening_files() {
        let (kind, error) = classify_and_check(Path::new("movie.mkv"), Some(0));
        assert_eq!(kind, Kind::Video);
        assert!(error.unwrap().contains("空文件"));

        let (_, error) = classify_and_check(Path::new("movie.mkv"), Some(512));
        assert!(error.unwrap().contains("文件过小"));

        let (_, error) = classify_and_check(Path::new("movie.mkv"), Some(2048));
        assert!(error.is_none());

        // 只扫描目录结构时没有大小信息，不做大小检查。
        let (_, error) = classify_and_check(Path::new("movie.mkv"), None);
        assert!(error.is_none());

        let (kind, error) = classify_and_check(Path::new("movie.mkv.part"), Some(0));
        assert_eq!(kind, Kind::Incomplete);
        assert!(error.is_none());
    }

    #[test]
    fn structure_scan_does_not_read_sizes() {
        let dir = std::env::temp_dir().join(format!(
            "embydown-review-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("电影")).unwrap();
        fs::write(dir.join("电影/空文件.mkv"), []).unwrap();

        let structure = scan_structure(&dir, None);
        assert_eq!(structure.files.len(), 1);
        assert!(structure.files.iter().all(|file| file.size.is_none()));
        assert!(structure.total_size.is_none());
        // 没有读取大小，就不做空文件/过小检查。
        assert!(structure.errors().next().is_none());

        let sized = scan(&dir);
        assert_eq!(sized.total_size, Some(0));
        assert!(sized.errors().next().is_some());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parent_scopes_collect_temp_directories() {
        let dir = std::env::temp_dir().join(format!(
            "embydown-review-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("影视/电影/片名")).unwrap();
        fs::create_dir_all(dir.join("影视/剧集/某剧/Season 00001")).unwrap();
        fs::write(dir.join("影视/电影/片名/正片.mkv"), []).unwrap();
        fs::write(
            dir.join("影视/剧集/某剧/Season 00001/某剧 - S00001E00001.mkv"),
            [],
        )
        .unwrap();

        let scopes = parent_scopes(&scan(&dir));
        assert_eq!(scopes.len(), 2);
        assert!(scopes.contains(&PathBuf::from("影视/电影/片名")));
        assert!(scopes.contains(&PathBuf::from("影视/剧集/某剧/Season 00001")));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scoped_scan_only_reads_temp_parent_dirs() {
        let dir = std::env::temp_dir().join(format!(
            "embydown-review-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("影视/电影/片名/子目录")).unwrap();
        fs::create_dir_all(dir.join("影视/电影/别的")).unwrap();
        fs::create_dir_all(dir.join("来自：分享")).unwrap();
        fs::write(dir.join("影视/电影/片名/正片.mkv"), []).unwrap();
        fs::write(dir.join("影视/电影/片名/花絮.mkv"), []).unwrap();
        fs::write(dir.join("影视/电影/片名/子目录/深层.mkv"), []).unwrap();
        fs::write(dir.join("影视/电影/别的/其它.mkv"), []).unwrap();
        fs::write(dir.join("来自：分享/别的.mkv"), []).unwrap();

        let dirs: BTreeSet<PathBuf> = [PathBuf::from("影视/电影/片名")].into_iter().collect();
        let report = scan_structure(&dir, Some(&dirs));
        let rels: Vec<&Path> = report.files.iter().map(|file| file.rel.as_path()).collect();
        assert_eq!(rels.len(), 2);
        assert!(rels.contains(&Path::new("影视/电影/片名/正片.mkv")));
        assert!(rels.contains(&Path::new("影视/电影/片名/花絮.mkv")));
        // 只读直属文件，不递归、不碰其它目录。
        assert!(!rels.contains(&Path::new("影视/电影/片名/子目录/深层.mkv")));
        assert!(!rels.contains(&Path::new("影视/电影/别的/其它.mkv")));
        assert!(!rels.contains(&Path::new("来自：分享/别的.mkv")));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn groups_plan_files_by_series_and_season() {
        let file = |rel: &str, size: u64| PlanFile {
            rel: PathBuf::from(rel),
            size: Some(size),
        };
        let files = vec![
            file("影视/剧集/某剧 (2020)/Season 00001/某剧 - S00001E00002.mkv", 100),
            file("影视/剧集/某剧 (2020)/Season 00001/某剧 - S00001E00001.mkv", 50),
            file("影视/电影/某电影 (2019)/某电影.mkv", 200),
        ];
        let rows = group_plan_files(&files);
        assert_eq!(rows.len(), 2);
        let series = rows.iter().find(|row| row.group.is_some()).unwrap();
        assert_eq!(series.files.len(), 2);
        assert_eq!(series.group.as_ref().unwrap().0, "某剧");
        assert_eq!(series.group.as_ref().unwrap().1, 1);

        let lines = render_plan_rows(&rows, false);
        assert!(lines.iter().any(|line| line == "某剧 S01（2 集，150 B）"));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("某电影 (2019)/某电影.mkv"))
        );

        let expanded = render_plan_rows(&rows, true);
        assert_eq!(expanded.len(), 3);
    }

    #[test]
    fn classifies_transient_mount_errors() {
        let error = anyhow::Error::from(std::io::Error::from_raw_os_error(131))
            .context("创建目录失败");
        assert!(is_transient_error(&error));
        let error = anyhow::Error::from(std::io::Error::from_raw_os_error(110));
        assert!(is_transient_error(&error));
        let error = anyhow::Error::from(std::io::Error::from_raw_os_error(2));
        assert!(!is_transient_error(&error));
    }

    #[test]
    fn targeted_plan_checks_only_pending_files() {
        let dir = std::env::temp_dir().join(format!(
            "embydown-review-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let temp = dir.join("downloads");
        let final_dir = dir.join("library");
        fs::create_dir_all(temp.join("影视/电影")).unwrap();
        fs::create_dir_all(final_dir.join("影视/电影")).unwrap();
        // 最终文件夹里与本次同步无关的内容，不应被扫描进计划。
        fs::create_dir_all(final_dir.join("来自：分享")).unwrap();
        fs::write(final_dir.join("来自：分享/别的电影.mkv"), [9u8; 64]).unwrap();

        fs::write(temp.join("影视/电影/一致.mkv"), [1u8; 2048]).unwrap();
        fs::write(final_dir.join("影视/电影/一致.mkv"), [2u8; 2048]).unwrap();
        fs::write(temp.join("影视/电影/覆盖.mkv"), [3u8; 2048]).unwrap();
        fs::write(final_dir.join("影视/电影/覆盖.mkv"), [4u8; 1024]).unwrap();
        fs::write(temp.join("影视/电影/新增.mkv"), [5u8; 2048]).unwrap();
        fs::write(temp.join("影视/电影/未完成.mkv.part"), [6u8; 2048]).unwrap();

        let temp_report = scan(&temp);
        let plan = build_plan_targeted(&temp_report, &final_dir, false);
        assert_eq!(plan.same, 1);
        assert_eq!(plan.overwrite.len(), 1);
        assert_eq!(plan.overwrite[0].rel, PathBuf::from("影视/电影/覆盖.mkv"));
        assert_eq!(plan.to_copy.len(), 1);
        assert_eq!(plan.to_copy[0].rel, PathBuf::from("影视/电影/新增.mkv"));
        assert_eq!(plan.skipped.len(), 1);
        assert!(plan.final_only.is_empty());

        let forced = build_plan_targeted(&temp_report, &final_dir, true);
        assert_eq!(forced.same, 0);
        assert_eq!(forced.overwrite.len(), 2);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn separates_nested_folders() {
        let dir = std::env::temp_dir().join(format!(
            "embydown-review-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let temp = dir.join("downloads");
        let final_dir = temp.join("library");
        fs::create_dir_all(&final_dir).unwrap();
        assert!(ensure_separate(&temp, &final_dir).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
