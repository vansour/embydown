//! 文件命名模板。
//!
//! 支持占位符：`{title} {series} {season} {season00} {season5} {episode}
//! {episode00} {episode5} {year} {seriesyear} {tmdbid} {tmdbidtag} {version}
//! {ext} {type} {id} {container}`。
//!
//! `{season00}` / `{episode00}` 至少两位补零，超过 99 时自动使用实际位数
//! （99 → `99`，100 → `100`）；`{season5}` / `{episode5}` 固定五位。
//!
//! `{tmdbid}` 输出 TMDB ID（剧集为所属剧集的 ID，缺失时为空）；
//! `{tmdbidtag}` 输出 ` [tmdbid-123]`（缺失时为空，不会留下空括号）。

use crate::model::{Item, MediaSource};
use crate::util::{sanitize_component, sanitize_path, truncate_component};

pub fn default_pattern(item: &Item) -> String {
    // 只有本次同时下载多个版本（--version all）时才用版本目录，
    // 避免单版本下载出现 `[Nekomoe kissaten@LoliHouse]` 之类的多余层级。
    let scoped = item.version_scoped
        && item
            .version
            .as_deref()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
    // Emby/Jellyfin 支持在名称后追加 `[tmdbid-123]` 提升识别准确率。
    // 剧集使用所属剧集的 TMDB ID；取不到时整段省略，避免出现空括号。
    let tag = tmdb_tag(item);
    let series_dir = format!("{{series}} ({{seriesyear}}){tag}");
    let movie_name = format!("{{title}} ({{year}}){tag}");
    match item.item_type.as_deref() {
        // Emby/Jellyfin 标准剧集结构：剧名 (年份) [tmdbid-123]/Season 01/剧名 - S01E01.ext
        // 季/集默认两位补零，超过 99 时自动扩展为三位（Season 100 / S100E100）。
        Some("Episode") if scoped => format!(
            "影视/剧集/{series_dir} [{{version}}]/Season {{season00}}/{{series}} - S{{season00}}E{{episode00}}.{{ext}}"
        ),
        Some("Episode") => format!(
            "影视/剧集/{series_dir}/Season {{season00}}/{{series}} - S{{season00}}E{{episode00}}.{{ext}}"
        ),
        // Emby 标准电影结构：电影名 (年份) [tmdbid-123]/电影名 (年份) [tmdbid-123].ext
        Some("Movie") if scoped => {
            format!("影视/电影/{movie_name} [{{version}}]/{movie_name}.{{ext}}")
        }
        Some("Movie") => format!("影视/电影/{movie_name}/{movie_name}.{{ext}}"),
        // 音频/其他类型（Video、MusicVideo、Trailer…）也必须带版本目录，
        // 否则 --version all 时多个版本会解析到同一路径互相覆盖。
        Some("Audio") if scoped => "影视/音乐/{title} [{version}]/{title}.{ext}".to_string(),
        Some("Audio") => "影视/音乐/{title}.{ext}".to_string(),
        _ if scoped => "影视/其他/{title} [{version}]/{title}.{ext}".to_string(),
        _ => "影视/其他/{title}.{ext}".to_string(),
    }
}

/// ` [tmdbid-123]`：没有 TMDB ID 时返回空串，模板里不会留下空括号。
fn tmdb_tag(item: &Item) -> String {
    match item.tmdb_id() {
        Some(id) => format!(" [tmdbid-{id}]"),
        None => String::new(),
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

/// 至少两位补零；超过 99 时按实际位数输出（99 → `99`，100 → `100`）。
fn pad2(n: i32) -> String {
    format!("{:02}", n.max(0))
}

/// 固定五位补零。
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

    let vars: [(&str, String); 17] = [
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
        ("tmdbid", item.tmdb_id().unwrap_or_default().to_string()),
        ("tmdbidtag", tmdb_tag(item)),
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
///
/// `disambiguator` 用于同一语言存在多条字幕轨道时区分（如 `chi.2`、`chi.简体`），
/// 否则后一条会覆盖/跳过前一条。
pub fn subtitle_path(
    video_rel: &str,
    lang: &str,
    ext: &str,
    forced: bool,
    disambiguator: Option<&str>,
) -> String {
    let base = video_rel.rsplit_once('.').map(|(b, _)| b).unwrap_or(video_rel);
    let lang = if lang.trim().is_empty() { "und" } else { lang };
    let mut suffix = String::new();
    if let Some(d) = disambiguator.map(str::trim).filter(|d| !d.is_empty()) {
        suffix.push('.');
        suffix.push_str(&sanitize_component(d));
    }
    if forced {
        suffix.push_str(".forced");
    }
    format!(
        "{}.{}{}.{}",
        base,
        sanitize_component(lang),
        suffix,
        sanitize_component(ext)
    )
}

/// 把 Emby 的 `MediaStream.Codec` 映射成播放器/媒体库能识别的字幕扩展名。
///
/// Emby 对外挂字幕常返回 `subrip`，若直接当扩展名会得到 `xxx.subrip`，
/// Emby/Jellyfin、mpv、VLC 都不会把该文件关联到视频。
pub fn subtitle_ext(codec: Option<&str>) -> String {
    match codec.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "ass" | "ssa" => "ass".into(),
        "webvtt" | "vtt" => "vtt".into(),
        "ttml" | "dfxp" => "ttml".into(),
        "hdmv_pgs_subtitle" | "pgs" | "pgssub" => "sup".into(),
        "dvd_subtitle" | "dvdsub" | "vobsub" => "sub".into(),
        // srt / subrip / mov_text / sami / microdvd 等纯文本格式统一请求 srt
        _ => "srt".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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
        let rel = render(&default_pattern(&item), &item, "mkv");
        assert_eq!(rel, "影视/剧集/示例剧 (2023)/Season 01/示例剧 - S01E02.mkv");
    }

    #[test]
    fn season_and_episode_pad_to_two_then_expand() {
        let pattern = "{season00}/{episode00}/S{season00}E{episode00}.{ext}";
        let mut item = episode();

        item.parent_index_number = Some(9);
        item.index_number = Some(9);
        assert_eq!(render(pattern, &item, "mkv"), "09/09/S09E09.mkv");

        item.parent_index_number = Some(99);
        item.index_number = Some(99);
        assert_eq!(render(pattern, &item, "mkv"), "99/99/S99E99.mkv");

        item.parent_index_number = Some(100);
        item.index_number = Some(120);
        assert_eq!(render(pattern, &item, "mkv"), "100/120/S100E120.mkv");

        item.parent_index_number = Some(1000);
        item.index_number = Some(1001);
        assert_eq!(render(pattern, &item, "mkv"), "1000/1001/S1000E1001.mkv");
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
        let rel = render(&default_pattern(&item), &item, "mp4");
        assert_eq!(rel, "影视/电影/101次求婚 (2013)/101次求婚 (2013).mp4");
    }

    #[test]
    fn movie_path_includes_tmdb_tag() {
        let item = Item {
            id: "m4".into(),
            name: Some("瞬息全宇宙".into()),
            item_type: Some("Movie".into()),
            production_year: Some(2022),
            container: Some("mkv".into()),
            provider_ids: Some(BTreeMap::from([("Tmdb".to_string(), "545611".to_string())])),
            ..Default::default()
        };
        let rel = render(&default_pattern(&item), &item, "mkv");
        assert_eq!(
            rel,
            "影视/电影/瞬息全宇宙 (2022) [tmdbid-545611]/瞬息全宇宙 (2022) [tmdbid-545611].mkv"
        );
    }

    #[test]
    fn series_folder_uses_series_tmdb_not_episode_tmdb() {
        let mut item = episode();
        item.provider_ids = Some(BTreeMap::from([("Tmdb".to_string(), "99999".to_string())]));
        item.series_tmdb_id = Some("456".into());
        let rel = render(&default_pattern(&item), &item, "mkv");
        assert_eq!(
            rel,
            "影视/剧集/示例剧 (2023) [tmdbid-456]/Season 01/示例剧 - S01E02.mkv"
        );

        // 没有剧集 TMDB ID 时不使用单集 ID，也不留下空括号。
        item.series_tmdb_id = None;
        let rel = render(&default_pattern(&item), &item, "mkv");
        assert_eq!(rel, "影视/剧集/示例剧 (2023)/Season 01/示例剧 - S01E02.mkv");
    }

    #[test]
    fn tmdb_placeholders_render_and_collapse() {
        let mut item = episode();
        item.series_tmdb_id = Some("456".into());
        assert_eq!(
            render("{title}{tmdbidtag}", &item, "mkv"),
            "试播集_ 一切开始 [tmdbid-456]"
        );
        assert_eq!(render("{tmdbid}", &item, "mkv"), "456");

        item.series_tmdb_id = None;
        assert_eq!(
            render("{title}{tmdbidtag}", &item, "mkv"),
            "试播集_ 一切开始"
        );
    }

    #[test]
    fn scoped_movie_keeps_tmdb_tag_before_version() {
        let item = Item {
            id: "m5".into(),
            name: Some("杯雪·一战成名".into()),
            item_type: Some("Movie".into()),
            production_year: Some(2026),
            container: Some("mkv".into()),
            provider_ids: Some(BTreeMap::from([("Tmdb".to_string(), "888".to_string())])),
            version: Some("2160p".into()),
            version_scoped: true,
            ..Default::default()
        };
        let rel = render(&default_pattern(&item), &item, "mkv");
        assert_eq!(
            rel,
            "影视/电影/杯雪·一战成名 (2026) [tmdbid-888] [2160p]/杯雪·一战成名 (2026) [tmdbid-888].mkv"
        );
    }

    #[test]
    fn long_chinese_name_fits_name_max() {
        let mut item = episode();
        item.name = Some("很长的标题".repeat(40)); // 200 字 = 600 字节
        let rel = render(&default_pattern(&item), &item, "mkv");
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
        let rel = render(&default_pattern(&item), &item, "mp4");
        assert_eq!(
            rel,
            "影视/剧集/葬送的芙莉莲 (2023)/Season 01/葬送的芙莉莲 - S01E16.mp4"
        );
    }

    #[test]
    fn single_version_selection_has_no_version_dir() {
        let mut item = episode();
        item.version = Some("Nekomoe kissaten@LoliHouse".into());
        let rel = render(&default_pattern(&item), &item, "mkv");
        assert_eq!(rel, "影视/剧集/示例剧 (2023)/Season 01/示例剧 - S01E02.mkv");
    }

    #[test]
    fn scoped_but_empty_version_does_not_create_empty_bracket() {
        let mut item = episode();
        item.version = Some("  ".into());
        item.version_scoped = true;
        let rel = render(&default_pattern(&item), &item, "mkv");
        assert_eq!(rel, "影视/剧集/示例剧 (2023)/Season 01/示例剧 - S01E02.mkv");
    }

    #[test]
    fn scoped_episode_uses_version_dir() {
        let mut item = episode();
        item.version = Some("Nekomoe kissaten@LoliHouse".into());
        item.version_scoped = true;
        let rel = render(&default_pattern(&item), &item, "mkv");
        assert_eq!(
            rel,
            "影视/剧集/示例剧 (2023) [Nekomoe kissaten@LoliHouse]/Season 01/示例剧 - S01E02.mkv"
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
        let rel = render(&default_pattern(&item), &item, "mkv");
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
        let rel = render(&default_pattern(&item), &item, "mkv");
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
            subtitle_path("示例剧/Season 01/a - S01E02 - t.mkv", "zh-CN", "srt", false, None),
            "示例剧/Season 01/a - S01E02 - t.zh-CN.srt"
        );
        assert_eq!(
            subtitle_path("a.mkv", "chi", "srt", false, Some("2")),
            "a.chi.2.srt"
        );
        assert_eq!(
            subtitle_path("a.mkv", "chi", "srt", true, None),
            "a.chi.forced.srt"
        );
    }

    #[test]
    fn subtitle_codec_maps_to_known_extension() {
        assert_eq!(subtitle_ext(Some("subrip")), "srt");
        assert_eq!(subtitle_ext(Some("SSA")), "ass");
        assert_eq!(subtitle_ext(Some("webvtt")), "vtt");
        assert_eq!(subtitle_ext(None), "srt");
    }
}
