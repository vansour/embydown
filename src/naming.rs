//! 文件命名模板。
//!
//! 支持占位符：`{title} {series} {season} {season00} {season5} {episode}
//! {episode00} {episode5} {year} {seriesyear} {version} {ext} {type} {id} {container}`。

use crate::model::{Item, MediaSource};
use crate::util::{sanitize_component, sanitize_path, truncate_component};

pub fn default_pattern(item: &Item) -> &'static str {
    // 只有本次同时下载多个版本（--version all）时才用版本目录，
    // 避免单版本下载出现 `[Nekomoe kissaten@LoliHouse]` 之类的多余层级。
    let scoped = item.version.is_some() && item.version_scoped;
    match item.item_type.as_deref() {
        // Emby/Jellyfin 标准剧集结构：剧名 (年份)/Season 00001/剧名 - S00001E00001.ext
        Some("Episode") if scoped => {
            "影视/剧集/{series} ({seriesyear}) [{version}]/Season {season5}/{series} - S{season5}E{episode5}.{ext}"
        }
        Some("Episode") => {
            "影视/剧集/{series} ({seriesyear})/Season {season5}/{series} - S{season5}E{episode5}.{ext}"
        }
        // Emby 标准电影结构：电影名 (年份)/电影名 (年份).ext
        Some("Movie") if scoped => {
            "影视/电影/{title} ({year}) [{version}]/{title} ({year}).{ext}"
        }
        Some("Movie") => "影视/电影/{title} ({year})/{title} ({year}).{ext}",
        Some("Audio") => "影视/音乐/{title}.{ext}",
        _ => "影视/其他/{title}.{ext}",
    }
}

/// 多版本条目的版本名：从媒体源文件名里去掉片名/剧名/季集编号/年份后的剩余部分，
/// 例如 `片名 (2026) - 2160p - WEB-DL.mkv` → `2160p - WEB-DL`，
/// `剧名 S01E01 - 1080p.mkv` → `1080p`。
///
/// 提取不到时返回空串，调用方可用 [`version_label`] 得到按分辨率/编码兜底的名字。
pub fn version_label_opt(item: &Item, source: &MediaSource) -> Option<String> {
    let raw = source
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            source
                .path
                .as_deref()
                .map(|p| p.rsplit(['/', '\\']).next().unwrap_or(p))
        })?;
    let stem = strip_media_ext(raw);
    let mut rest = stem.to_string();

    // 先去掉季集编号/年份，避免片名本身带 SxxExx 时匹配不上
    rest = strip_episode_tags(&rest);
    rest = strip_cn_episode(&rest);
    rest = strip_year_parens(&rest);

    // 去掉剧名、剧集目录名、季名、单集标题（可能出现在任意位置）
    let series = item
        .series_name
        .clone()
        .or_else(|| source.path.as_deref().and_then(series_from_path))
        .or_else(|| item.path.as_deref().and_then(series_from_path));
    let title = item.title();
    let mut tokens: Vec<&str> = vec![title.as_str()];
    if let Some(s) = series.as_deref() {
        tokens.push(s);
    }
    if let Some(s) = item.season_name.as_deref() {
        tokens.push(s);
    }
    for token in tokens {
        let token = token.trim();
        if !token.is_empty() {
            rest = rest.replace(token, " ");
        }
    }

    let label = rest
        .trim_matches(|c: char| {
            c.is_whitespace()
                || matches!(c, '-' | '_' | '.' | ',' | '·' | '(' | ')' | '[' | ']' | '【' | '】')
        })
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    if label.is_empty() || label.eq_ignore_ascii_case(&title) {
        return None;
    }
    Some(label)
}

/// 版本名兜底：文件名里没有版本信息时，用分辨率 + 视频编码（再不行用序号）。
pub fn version_label(item: &Item, source: &MediaSource, index: usize) -> String {
    if let Some(label) = version_label_opt(item, source) {
        return label;
    }
    let streams = source
        .media_streams
        .as_deref()
        .or(item.media_streams.as_deref());
    let mut parts = Vec::new();
    if let Some(v) = streams.and_then(|ss| {
        ss.iter()
            .find(|s| s.stream_type.as_deref() == Some("Video"))
    }) {
        if let Some(h) = v.height.filter(|h| *h > 0) {
            parts.push(format!("{h}p"));
        }
        if let Some(c) = v.codec.as_deref().filter(|c| !c.is_empty()) {
            parts.push(c.to_string());
        }
    }
    if parts.is_empty() {
        format!("版本{}", index + 1)
    } else {
        parts.join(" ")
    }
}

/// 去掉常见媒体后缀（`mkv` / `mp4` / `m2ts` …），避免把标题里的点误当后缀。
fn strip_media_ext(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty()
                && ext.len() <= 4
                && ext.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            stem
        }
        _ => name,
    }
}

/// 去掉 `第 1 集` / `第12话` 这类中/日文集数标记。
fn strip_cn_episode(s: &str) -> String {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut out = String::new();
    let mut last = 0;
    let mut i = 0;
    while i < chars.len() {
        if chars[i].1 == '第' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].1.is_whitespace() {
                j += 1;
            }
            let digit_start = j;
            while j < chars.len() && chars[j].1.is_ascii_digit() {
                j += 1;
            }
            if j > digit_start {
                let mut k = j;
                while k < chars.len() && chars[k].1.is_whitespace() {
                    k += 1;
                }
                if k < chars.len() && matches!(chars[k].1, '集' | '话' | '話' | '期') {
                    let end = if k + 1 < chars.len() {
                        chars[k + 1].0
                    } else {
                        s.len()
                    };
                    out.push_str(&s[last..chars[i].0]);
                    last = end;
                    i = k + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&s[last..]);
    out
}

/// 去掉 `S01E02` 之类的季集标记。
fn strip_episode_tags(s: &str) -> String {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut out = String::new();
    let mut last = 0;
    let mut i = 0;
    while i < chars.len() {
        let (pos, c) = chars[i];
        if (c == 'S' || c == 's') && i + 1 < chars.len() && chars[i + 1].1.is_ascii_digit() {
            let mut j = i + 1;
            while j < chars.len() && chars[j].1.is_ascii_digit() {
                j += 1;
            }
            if j < chars.len() && (chars[j].1 == 'E' || chars[j].1 == 'e') {
                let mut k = j + 1;
                while k < chars.len() && chars[k].1.is_ascii_digit() {
                    k += 1;
                }
                if k > j + 1 {
                    let end = if k < chars.len() { chars[k].0 } else { s.len() };
                    out.push_str(&s[last..pos]);
                    last = end;
                    i = k;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&s[last..]);
    out
}

/// 去掉 `(2026)` 这类括号年份。
fn strip_year_parens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('(') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        if let Some(end) = after.find(')') {
            let inside = &after[..end];
            if inside.len() == 4 && inside.chars().all(|c| c.is_ascii_digit()) {
                rest = &after[end + 1..];
                continue;
            }
        }
        out.push('(');
        rest = after;
    }
    out.push_str(rest);
    out
}

fn pad2(n: i32) -> String {
    format!("{:02}", n.max(0))
}

fn pad5(n: i32) -> String {
    format!("{:05}", n.max(0))
}

/// 从服务器路径推断剧名：取 `Season xx` 目录的上一级，去掉结尾的年份。
fn series_from_path(path: &str) -> Option<String> {
    let segs: Vec<&str> = path.split(['/', '\\']).collect();
    for (i, seg) in segs.iter().enumerate() {
        let lower = seg.trim().to_ascii_lowercase();
        if lower.starts_with("season") && i > 0 {
            let parent = segs[i - 1].trim();
            let name = match parent.rsplit_once('(') {
                Some((head, tail))
                    if tail.trim_end_matches(')').chars().all(|c| c.is_ascii_digit()) =>
                {
                    head.trim()
                }
                _ => parent,
            };
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// 从服务器路径的剧集目录推断年份，如 `剧名 (2023)/Season 1` → 2023。
fn series_year_from_path(path: &str) -> Option<i32> {
    let segs: Vec<&str> = path.split(['/', '\\']).collect();
    for (i, seg) in segs.iter().enumerate() {
        let lower = seg.trim().to_ascii_lowercase();
        if lower.starts_with("season") && i > 0 {
            let parent = segs[i - 1].trim();
            if let Some((_, tail)) = parent.rsplit_once('(') {
                let digits = tail.trim_end_matches(')').trim();
                if !digits.is_empty()
                    && digits.chars().all(|c| c.is_ascii_digit())
                    && let Ok(year) = digits.parse::<i32>()
                {
                    return Some(year);
                }
            }
        }
    }
    None
}

/// 渲染模板 → 相对路径（已清洗，可直接 join 到输出目录）。
pub fn render(pattern: &str, item: &Item, ext: &str) -> String {
    let title = item.title();
    let series = item
        .series_name
        .clone()
        .filter(|s| !s.trim().is_empty())
        // 部分服务器列表接口不返回 SeriesName，从服务器路径兜底
        .or_else(|| item.path.as_deref().and_then(series_from_path))
        .unwrap_or_else(|| title.clone());
    let year = item
        .production_year
        .map(|y| y.to_string())
        .unwrap_or_else(|| "未知年份".to_string());
    let series_year = item
        .series_year
        .or_else(|| item.path.as_deref().and_then(series_year_from_path))
        .or(item.production_year)
        .map(|y| y.to_string())
        .unwrap_or_else(|| "未知年份".to_string());
    let season = item.season_no();
    let episode = item.episode_no();

    let vars: [(&str, String); 15] = [
        ("title", title.clone()),
        ("series", series.clone()),
        ("season", season.to_string()),
        ("season00", pad2(season)),
        ("season5", pad5(season)),
        ("episode", episode.to_string()),
        ("episode00", pad2(episode)),
        ("episode5", pad5(episode)),
        ("year", year),
        ("seriesyear", series_year),
        ("version", item.version.clone().unwrap_or_default()),
        ("ext", ext.to_string()),
        ("type", item.display_type().to_string()),
        ("id", item.id.clone()),
        ("container", ext.to_string()),
    ];

    let mut out = pattern.to_string();
    for (key, val) in vars {
        out = out.replace(&format!("{{{}}}", key), &val);
    }

    // 收尾：清掉没有替换到的未知占位符，避免出现字面 `{foo}`。
    while let Some(start) = out.find('{') {
        match out[start..].find('}') {
            Some(rel_end) => {
                let end = start + rel_end;
                out.replace_range(start..=end, "");
            }
            None => break,
        }
    }

    let cleaned = sanitize_path(&out);
    // 单段过长会打不开：逐段截断。
    cleaned
        .split('/')
        .map(|seg| truncate_component(seg, 200))
        .collect::<Vec<_>>()
        .join("/")
        .trim_start_matches('/')
        .to_string()
}

/// 字幕文件相对路径：与视频同名，追加语言与字幕格式。
pub fn subtitle_path(video_rel: &str, lang: &str, ext: &str, forced: bool) -> String {
    let base = video_rel.rsplit_once('.').map(|(b, _)| b).unwrap_or(video_rel);
    let lang = if lang.trim().is_empty() { "und" } else { lang };
    let suffix = if forced { ".forced" } else { "" };
    format!(
        "{}.{}{}.{}",
        base,
        sanitize_component(lang),
        suffix,
        sanitize_component(ext)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn episode() -> Item {
        Item {
            id: "abc".into(),
            name: Some("试播集: 一切开始".into()),
            item_type: Some("Episode".into()),
            series_name: Some("示例剧".into()),
            parent_index_number: Some(1),
            index_number: Some(2),
            production_year: Some(2023),
            container: Some("mkv".into()),
            ..Default::default()
        }
    }

    #[test]
    fn renders_episode_path() {
        let item = episode();
        let rel = render(default_pattern(&item), &item, "mkv");
        assert_eq!(
            rel,
            "影视/剧集/示例剧 (2023)/Season 00001/示例剧 - S00001E00002.mkv"
        );
    }

    #[test]
    fn renders_movie_path() {
        let item = Item {
            id: "m1".into(),
            name: Some("101次求婚".into()),
            item_type: Some("Movie".into()),
            production_year: Some(2013),
            container: Some("mp4".into()),
            ..Default::default()
        };
        let rel = render(default_pattern(&item), &item, "mp4");
        assert_eq!(rel, "影视/电影/101次求婚 (2013)/101次求婚 (2013).mp4");
    }

    #[test]
    fn long_chinese_name_fits_name_max() {
        let mut item = episode();
        item.name = Some("很长的标题".repeat(40)); // 200 字 = 600 字节
        let rel = render(default_pattern(&item), &item, "mkv");
        let last = rel.rsplit('/').next().unwrap();
        assert!(last.len() <= 220, "单段 {} 字节，超过 NAME_MAX", last.len());
    }

    #[test]
    fn unknown_placeholder_removed() {
        let item = episode();
        let rel = render("{title}-{nope}", &item, "mkv");
        assert_eq!(rel, "试播集_ 一切开始-");
    }

    #[test]
    fn falls_back_to_path_for_series_and_season() {
        let item = Item {
            id: "x".into(),
            name: Some("长寿的朋友".into()),
            item_type: Some("Episode".into()),
            index_number: Some(16),
            path: Some(
                "/Emby/动漫/葬送的芙莉莲 (2023)/Season 1/葬送的芙莉莲 - S01E16 - 第16集 - ANi.mp4"
                    .into(),
            ),
            container: Some("mp4".into()),
            ..Default::default()
        };
        let rel = render(default_pattern(&item), &item, "mp4");
        assert_eq!(
            rel,
            "影视/剧集/葬送的芙莉莲 (2023)/Season 00001/葬送的芙莉莲 - S00001E00016.mp4"
        );
    }

    #[test]
    fn single_version_selection_has_no_version_dir() {
        let mut item = episode();
        item.version = Some("Nekomoe kissaten@LoliHouse".into());
        let rel = render(default_pattern(&item), &item, "mkv");
        assert_eq!(
            rel,
            "影视/剧集/示例剧 (2023)/Season 00001/示例剧 - S00001E00002.mkv"
        );
    }

    #[test]
    fn scoped_episode_uses_version_dir() {
        let mut item = episode();
        item.version = Some("Nekomoe kissaten@LoliHouse".into());
        item.version_scoped = true;
        let rel = render(default_pattern(&item), &item, "mkv");
        assert_eq!(
            rel,
            "影视/剧集/示例剧 (2023) [Nekomoe kissaten@LoliHouse]/Season 00001/示例剧 - S00001E00002.mkv"
        );
    }

    #[test]
    fn renders_movie_with_version_scope() {
        let item = Item {
            id: "m2".into(),
            name: Some("杯雪·一战成名".into()),
            item_type: Some("Movie".into()),
            production_year: Some(2026),
            container: Some("mkv".into()),
            version: Some("2160p - WEB-DL HDR".into()),
            version_scoped: true,
            ..Default::default()
        };
        let rel = render(default_pattern(&item), &item, "mkv");
        assert_eq!(
            rel,
            "影视/电影/杯雪·一战成名 (2026) [2160p - WEB-DL HDR]/杯雪·一战成名 (2026).mkv"
        );
    }

    #[test]
    fn single_version_movie_has_no_version_dir() {
        let item = Item {
            id: "m3".into(),
            name: Some("杯雪·一战成名".into()),
            item_type: Some("Movie".into()),
            production_year: Some(2026),
            container: Some("mkv".into()),
            version: Some("2160p - WEB-DL HDR".into()),
            ..Default::default()
        };
        let rel = render(default_pattern(&item), &item, "mkv");
        assert_eq!(rel, "影视/电影/杯雪·一战成名 (2026)/杯雪·一战成名 (2026).mkv");
    }

    #[test]
    fn version_label_from_episode_source_name() {
        let item = Item {
            name: Some("第一集".into()),
            item_type: Some("Episode".into()),
            series_name: Some("庆余年".into()),
            ..Default::default()
        };
        let source = MediaSource {
            name: Some("庆余年 S01E01 - 2160p - WEB-DL.mkv".into()),
            ..Default::default()
        };
        assert_eq!(version_label(&item, &source, 1), "2160p - WEB-DL");
    }

    #[test]
    fn version_label_from_full_source_filename() {
        let item = Item {
            name: Some("他人的故乡".into()),
            item_type: Some("Episode".into()),
            series_name: Some("葬送的芙莉莲".into()),
            ..Default::default()
        };
        let source = MediaSource {
            name: Some(
                "葬送的芙莉莲 - S01E32 - 第32集 - Nekomoe kissaten@LoliHouse.mkv".into(),
            ),
            ..Default::default()
        };
        assert_eq!(
            version_label_opt(&item, &source).as_deref(),
            Some("Nekomoe kissaten@LoliHouse")
        );
    }

    #[test]
    fn version_label_uses_path_series_when_name_is_filename() {
        let item = Item {
            name: Some("葬送的芙莉莲 - S02E29 - 第29集 - ANi".into()),
            item_type: Some("Episode".into()),
            path: Some(
                "/Emby/动漫/葬送的芙莉莲 (2023)/Season 2/葬送的芙莉莲 - S02E29 - 第29集 - ANi.mp4"
                    .into(),
            ),
            ..Default::default()
        };
        let source = MediaSource {
            name: Some("葬送的芙莉莲 - S02E29 - 第29集 - ANi".into()),
            ..Default::default()
        };
        assert_eq!(version_label_opt(&item, &source).as_deref(), Some("ANi"));
    }

    #[test]
    fn version_label_from_spaced_episode_marker() {
        let item = Item {
            name: Some("冒险的结束".into()),
            item_type: Some("Episode".into()),
            series_name: Some("葬送的芙莉莲".into()),
            ..Default::default()
        };
        let source = MediaSource {
            name: Some("葬送的芙莉莲 - S01E01 - 第 1 集 - 1080p - BluRay.mkv".into()),
            ..Default::default()
        };
        assert_eq!(
            version_label_opt(&item, &source).as_deref(),
            Some("1080p - BluRay")
        );
    }

    #[test]
    fn no_version_label_from_episode_title_only() {
        let item = Item {
            name: Some("为保送学子，各路权贵贿赂范闲".into()),
            item_type: Some("Episode".into()),
            series_name: Some("庆余年".into()),
            ..Default::default()
        };
        let source = MediaSource {
            name: Some("庆余年 - S02E17 - 为保送学子，各路权贵贿赂范闲.mp4".into()),
            ..Default::default()
        };
        assert_eq!(version_label_opt(&item, &source), None);
    }

    #[test]
    fn subtitle_naming() {
        assert_eq!(
            subtitle_path("示例剧/Season 01/a - S01E02 - t.mkv", "zh-CN", "srt", false),
            "示例剧/Season 01/a - S01E02 - t.zh-CN.srt"
        );
    }
}
