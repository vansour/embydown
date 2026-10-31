//! 未完成任务扫描与来源匹配。
//!
//! 下载目录里的 `<文件名>.part` / `<文件名>.part.json` 代表一个未完成任务。
//! 这里负责：
//! 1. 递归扫描下载目录，得到所有未完成的目标文件；
//! 2. 从文件名推导搜索关键字（剧集取剧名，电影去掉年份/质量标签）；
//! 3. 在所有服务器上搜索，必要时展开 Series/Season 找到同一季集的分集；
//! 4. 返回候选来源供用户选择（交互模式会把选中的条目写回原来的目标路径续传）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::emby::Client;
use crate::model::{self, Item};

/// 一次最多处理多少个未完成任务（避免误选一个巨大的目录后大量请求服务器）。
pub const MAX_TASKS: usize = 30;
/// 每个任务最多保留多少个候选来源。
pub const MAX_CANDIDATES: usize = 20;
/// 单个服务器搜索时最多取多少条。
const SEARCH_LIMIT: u32 = 20;
/// 递归深度上限（防止异常目录结构）。
const MAX_DEPTH: usize = 16;

/// 下载目录里的一个未完成任务。
#[derive(Debug, Clone)]
pub struct Unfinished {
    /// 完成后的目标路径（已去掉 `.part` / `.part.json`）。
    pub dest: PathBuf,
    /// `.part` 当前占用的字节数。
    pub partial_bytes: u64,
    /// 从文件名推导出的搜索关键字。
    pub keyword: String,
    /// 文件名里解析出的季/集；解析不到时为 None（电影等）。
    pub season_episode: Option<(i32, i32)>,
}

/// 一个可选的下载来源。
#[derive(Debug, Clone)]
pub struct Candidate {
    pub server: String,
    pub item: Item,
    pub score: i64,
}

/// 一个未完成任务及其匹配结果。
#[derive(Debug, Clone)]
pub struct Task {
    pub file: Unfinished,
    pub candidates: Vec<Candidate>,
    /// 搜索/展开过程中遇到的非致命错误（仍可能有其它候选）。
    pub errors: Vec<String>,
}

/// 扫描结果。
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub tasks: Vec<Task>,
    /// 扫描到的未完成任务总数（可能大于 tasks.len()）。
    pub total: usize,
}

/// 扫描下载目录并搜索所有未完成任务。
pub async fn scan_and_search(
    out: &Path,
    clients: &[(String, Client)],
) -> Result<ScanResult> {
    let files = scan(out)?;
    let total = files.len();
    let mut tasks = Vec::new();
    for file in files.into_iter().take(MAX_TASKS) {
        let (candidates, errors) =
            find_candidates(clients, &file.keyword, file.season_episode).await;
        tasks.push(Task {
            file,
            candidates,
            errors,
        });
    }
    Ok(ScanResult { tasks, total })
}

/// 递归扫描目录，返回未完成任务（按目标路径排序）。
pub fn scan(out: &Path) -> Result<Vec<Unfinished>> {
    let mut found: std::collections::BTreeMap<PathBuf, u64> = std::collections::BTreeMap::new();
    walk(out, &mut found, 0)
        .with_context(|| format!("扫描下载目录 {} 失败", out.display()))?;

    let mut tasks: Vec<Unfinished> = found
        .into_iter()
        .map(|(dest, partial_bytes)| {
            let (keyword, season_episode) = derive_keyword(&dest);
            Unfinished {
                dest,
                partial_bytes,
                keyword,
                season_episode,
            }
        })
        .collect();
    tasks.sort_by(|a, b| a.dest.cmp(&b.dest));
    Ok(tasks)
}

fn walk(dir: &Path, found: &mut std::collections::BTreeMap<PathBuf, u64>, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir)?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            // 子目录不可读不应让整个扫描失败。
            let _ = walk(&path, found, depth + 1);
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(base) = name.strip_suffix(".part.json") {
            let dest = path.with_file_name(base);
            found.entry(dest).or_insert(0);
        } else if let Some(base) = name.strip_suffix(".part") {
            let dest = path.with_file_name(base);
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            found
                .entry(dest)
                .and_modify(|s| *s = (*s).max(size))
                .or_insert(size);
        }
    }
    Ok(())
}

/// 从目标路径推导搜索关键字与季集。
pub fn derive_keyword(dest: &Path) -> (String, Option<(i32, i32)>) {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = dest
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.clone());

    let season_episode = model::parse_season_episode(&name);
    let keyword = if season_episode.is_some() {
        // 剧集：取 SxxEyy 之前的部分作为剧名。
        let series = series_prefix(&stem);
        let cleaned = cleanup_keyword(&series);
        if cleaned.is_empty() {
            fallback_keyword(dest, &stem)
        } else {
            cleaned
        }
    } else {
        let cleaned = cleanup_keyword(&stem);
        if cleaned.is_empty() {
            fallback_keyword(dest, &stem)
        } else {
            cleaned
        }
    };
    (keyword, season_episode)
}

fn fallback_keyword(dest: &Path, stem: &str) -> String {
    // 文件名没有可用信息时，用上一级目录名（例如 `片名 (2026)/video.mkv`）。
    dest.parent()
        .and_then(|p| p.file_name())
        .map(|n| cleanup_keyword(&n.to_string_lossy()))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let s = cleanup_keyword(stem);
            (!s.is_empty()).then_some(s)
        })
        .unwrap_or_else(|| "untitled".to_string())
}

/// 去掉 `SxxEyy` 及其后的内容。
fn series_prefix(stem: &str) -> String {
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
                while k < bytes.len() && matches!(bytes[k], b'.' | b'_' | b' ' | b'-') {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == b'e' {
                    let mut m = k + 1;
                    while m < bytes.len() && bytes[m].is_ascii_digit() {
                        m += 1;
                    }
                    if m > k + 1 {
                        return stem[..i].to_string();
                    }
                }
            }
        }
        i += 1;
    }
    stem.to_string()
}

/// 清理关键字：去掉末尾的年份/版本括号，把无空格名称里的点/下划线变成空格，
/// 并从第一个四位年份处截断，最后去掉常见质量标签。
fn cleanup_keyword(raw: &str) -> String {
    let mut s = raw.trim().to_string();

    // 去掉末尾的 (...) / [...]（年份、分辨率、版本等）。
    loop {
        let t = s.trim_end();
        let close = if t.ends_with(')') {
            ')'
        } else if t.ends_with(']') {
            ']'
        } else {
            break;
        };
        let Some(open) = t.rfind(if close == ')' { '(' } else { '[' }) else {
            break;
        };
        let inner = &t[open + 1..t.len() - 1];
        if inner.is_empty()
            || !inner
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-' || c == '.')
        {
            break;
        }
        s = t[..open].trim_end().to_string();
    }

    // 发布组常见的点分隔文件名：Spider-Man.No.Way.Home.2021.1080p
    if !s.contains(' ') {
        s = s.replace(['.', '_'], " ");
    }

    let tokens: Vec<&str> = s.split_whitespace().collect();
    // 从第一个四位年份开始截断。
    let end = tokens
        .iter()
        .position(|t| {
            let digits = t.trim_matches(|c: char| !c.is_ascii_digit());
            digits.len() == 4
                && digits
                    .parse::<i32>()
                    .map(|y| (1900..=2099).contains(&y))
                    .unwrap_or(false)
        })
        .unwrap_or(tokens.len());
    let mut tokens = tokens[..end].to_vec();

    // 末尾的质量/编码标签。
    const QUALITY: &[&str] = &[
        "2160p", "1080p", "720p", "480p", "web-dl", "webrip", "web", "bluray", "blu-ray",
        "bdrip", "brrip", "dvdrip", "remux", "hdr", "dv", "x264", "x265", "h264", "h265",
        "hevc", "avc", "aac", "dts", "truehd", "atmos", "proper", "repack",
    ];
    while let Some(last) = tokens.last() {
        if QUALITY.contains(&last.to_lowercase().as_str()) {
            tokens.pop();
        } else {
            break;
        }
    }

    tokens
        .join(" ")
        .trim_matches(|c: char| matches!(c, '-' | '_' | '.' | ' ' | '·'))
        .to_string()
}

/// 在所有服务器上为关键字寻找候选来源。
pub async fn find_candidates(
    clients: &[(String, Client)],
    keyword: &str,
    season_episode: Option<(i32, i32)>,
) -> (Vec<Candidate>, Vec<String>) {
    let mut out: Vec<Candidate> = Vec::new();
    let mut errors = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for (server, client) in clients {
        let hits = match client.search(keyword, SEARCH_LIMIT).await {
            Ok(items) => items,
            Err(e) => {
                errors.push(format!("{server}: {e:#}"));
                continue;
            }
        };
        for hit in hits {
            let score = hit.search_score(keyword);
            if score <= 0 {
                continue;
            }
            let was_container = hit.is_container();

            // 没有解析出季集时，无法知道容器里对应哪一集，跳过以免整季下载。
            if was_container && season_episode.is_none() {
                continue;
            }

            let items = if was_container {
                match crate::resolve_item(client, hit.clone(), &[]).await {
                    Ok(items) => items,
                    Err(e) => {
                        errors.push(format!("{server}: 展开 {} 失败: {e:#}", hit.title()));
                        continue;
                    }
                }
            } else {
                vec![hit]
            };

            for item in items {
                if let Some((season, episode)) = season_episode {
                    // 未完成任务是某一集：只保留同一季集。
                    if item.item_type.as_deref() != Some("Episode")
                        || item.season_no() != season
                        || item.episode_no() != episode
                    {
                        continue;
                    }
                } else if was_container {
                    continue;
                }
                if !item.is_video()
                    && item
                        .media_sources
                        .as_ref()
                        .map(|s| s.is_empty())
                        .unwrap_or(true)
                {
                    continue;
                }
                if seen.insert((server.clone(), item.id.clone())) {
                    out.push(Candidate {
                        server: server.clone(),
                        item,
                        score,
                    });
                }
            }
        }
    }

    out.sort_by_cached_key(|c| {
        (
            std::cmp::Reverse(c.score),
            c.item.title().chars().count(),
            c.item.title().to_lowercase(),
            c.server.clone(),
        )
    });
    out.truncate(MAX_CANDIDATES);
    (out, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "embydown-repair-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn episode_keyword_uses_series_name() {
        let dest = Path::new("/x/庆余年 (2019)/Season 00001/庆余年 - S00001E00001.mkv");
        let (keyword, se) = derive_keyword(dest);
        assert_eq!(keyword, "庆余年");
        assert_eq!(se, Some((1, 1)));
    }

    #[test]
    fn movie_keyword_strips_year() {
        let (keyword, se) = derive_keyword(Path::new("/x/影视/电影/Avatar (2009)/Avatar (2009).mkv"));
        assert_eq!(keyword, "Avatar");
        assert_eq!(se, None);
    }

    #[test]
    fn dotted_release_name_is_cleaned() {
        let (keyword, _) =
            derive_keyword(Path::new("/x/Spider-Man.No.Way.Home.2021.1080p.mkv"));
        assert_eq!(keyword, "Spider-Man No Way Home");
    }

    #[test]
    fn scan_finds_part_and_part_json_once() {
        let dir = temp_dir("scan");
        let sub = dir.join("影视/电影/片名 (2026)");
        std::fs::create_dir_all(&sub).unwrap();
        let dest = sub.join("片名 (2026).mkv");
        std::fs::write(dest.with_extension("mkv.part"), b"partial").unwrap();
        std::fs::write(dest.with_extension("mkv.part.json"), b"{}").unwrap();

        let tasks = scan(&dir).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].dest, dest);
        assert_eq!(tasks[0].partial_bytes, 7);
        assert_eq!(tasks[0].keyword, "片名");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_ignores_finished_files() {
        let dir = temp_dir("finished");
        std::fs::write(dir.join("done.mkv"), b"x").unwrap();
        assert!(scan(&dir).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
