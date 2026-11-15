//! 本地审查下载文件夹。
//!
//! 审查在本地完成，不访问服务器，也不打开文件读取内容：
//! - 依据目录项（文件名/路径与大小）统计视频/字幕/元数据/图片与总大小；
//! - 按 `SxxExx` 识别剧集，检查集号连续性（缺集）与重复集；
//! - 检查空文件、过小视频与 `.part` 等未完成文件。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;

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
    /// `None` 表示目录项读取不到文件大小。
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

/// `embydown review`：审查下载文件夹。
pub fn run(dir: Option<PathBuf>) -> Result<()> {
    let dir = resolve_dir(dir)?;
    let report = scan(&dir);

    print_title("本地审查");
    print_folder(&report, "下载文件夹");
    Ok(())
}

fn resolve_dir(dir: Option<PathBuf>) -> Result<PathBuf> {
    let dir = match dir {
        Some(path) => path,
        None => {
            let cfg = config::load_optional()?.unwrap_or_default();
            let configured = cfg.settings.download_dir.trim();
            PathBuf::from(if configured.is_empty() {
                "./downloads"
            } else {
                configured
            })
        }
    };
    util::expand_tilde(&dir)
}

/// 扫描文件夹并读取文件大小。
fn scan(root: &Path) -> FolderReport {
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
    walk(root, root, &mut files, 0, &mut scan_errors);
    summarize(root, files, scan_errors)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<FileEntry>, depth: usize, errors: &mut Vec<String>) {
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
            walk(root, &path, out, depth + 1, errors);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        // 网盘挂载上逐文件 stat 可能极慢；读不到大小就按未知处理。
        let size = entry.metadata().ok().map(|meta| meta.len());
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
    println!("\n[{label}] {}", report.root.display());
    print_folder_body(report);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "embydown-review-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

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
        let rel = Path::new("影视/剧集/庆余年 (2019)/Season 01/庆余年 - S01E02.mkv");
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

        // 读不到大小时不做大小检查。
        let (_, error) = classify_and_check(Path::new("movie.mkv"), None);
        assert!(error.is_none());

        let (kind, error) = classify_and_check(Path::new("movie.mkv.part"), Some(0));
        assert_eq!(kind, Kind::Incomplete);
        assert!(error.is_none());
    }

    #[test]
    fn scan_reads_sizes_and_flags_empty_files() {
        let dir = scratch("scan");
        fs::create_dir_all(dir.join("影视/电影/片名")).unwrap();
        fs::write(dir.join("影视/电影/片名/空文件.mkv"), []).unwrap();
        fs::write(dir.join("影视/电影/片名/正常.mkv"), [1u8; 2048]).unwrap();
        fs::write(dir.join("影视/电影/片名/正常.mkv.part"), [2u8; 64]).unwrap();

        let report = scan(&dir);
        assert_eq!(report.files.len(), 3);
        assert_eq!(report.videos, 2);
        assert_eq!(report.incomplete, 1);
        assert_eq!(report.incomplete_tasks, 1);
        assert_eq!(report.total_size, Some(2112));
        assert!(report.errors().any(|(_, error)| error.contains("空文件")));

        fs::remove_dir_all(&dir).unwrap();
    }
}
