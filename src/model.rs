//! Emby Web API 的数据模型（只声明用得到的字段，全部可选以容忍版本差异）。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct Item {
    pub id: String,
    pub name: Option<String>,
    /// BaseItemKind：Movie / Episode / Series / Season / BoxSet / Audio / Video / Folder / CollectionFolder …
    #[serde(rename = "Type")]
    pub item_type: Option<String>,
    /// Video / Audio / Photo …
    pub media_type: Option<String>,
    pub container: Option<String>,
    pub series_name: Option<String>,
    pub series_id: Option<String>,
    pub season_name: Option<String>,
    pub parent_id: Option<String>,
    pub index_number: Option<i32>,
    pub parent_index_number: Option<i32>,
    pub production_year: Option<i32>,
    pub run_time_ticks: Option<i64>,
    pub overview: Option<String>,
    /// 服务器上的真实路径（需要管理员或权限足够时才有）
    pub path: Option<String>,
    pub is_folder: Option<bool>,
    pub child_count: Option<i32>,
    /// 递归子项总数（Emby 对媒体库/合集给出这个更准）
    pub recursive_item_count: Option<i64>,
    pub media_sources: Option<Vec<MediaSource>>,
    pub media_streams: Option<Vec<MediaStream>>,
    /// 非 API 字段：直接给一个 http(s) 直链时用它，跳过 URL 构造
    #[serde(skip)]
    pub direct_url: Option<String>,
    /// 非 API 字段：展开剧集时从 Series 容器/路径补全的剧集年份
    #[serde(skip)]
    pub series_year: Option<i32>,
    /// 非 API 字段：多版本时选中的版本名称（用于文件名区分）
    #[serde(skip)]
    pub version: Option<String>,
    /// 非 API 字段：本次下载里同一条目存在多个版本，需要把版本名放进目录避免覆盖
    #[serde(skip)]
    pub version_scoped: bool,
}

impl Item {
    pub fn is_video(&self) -> bool {
        matches!(
            self.media_type.as_deref(),
            Some("Video") | Some("Audio")
        ) || matches!(
            self.item_type.as_deref(),
            Some("Movie") | Some("Episode") | Some("Video") | Some("MusicVideo") | Some("Trailer") | Some("Audio")
        )
    }

    /// 是否是需要递归展开的“文件夹型”条目。
    pub fn is_container(&self) -> bool {
        matches!(
            self.item_type.as_deref(),
            Some("Series")
                | Some("Season")
                | Some("BoxSet")
                | Some("CollectionFolder")
                | Some("Folder")
                | Some("MusicAlbum")
                | Some("MusicArtist")
                | Some("Playlist")
        ) || self.is_folder.unwrap_or(false) && !self.is_video()
    }

    pub fn size(&self) -> Option<u64> {
        self.media_sources
            .as_ref()
            .and_then(|s| s.first())
            .and_then(|s| s.size)
    }

    pub fn media_source_id(&self) -> Option<&str> {
        self.media_sources
            .as_ref()
            .and_then(|s| s.first())
            .and_then(|s| s.id.as_deref())
    }

    pub fn display_type(&self) -> &str {
        self.item_type.as_deref().unwrap_or("-")
    }

    pub fn ext(&self) -> String {
        if let Some(c) = &self.container
            && !c.is_empty()
        {
            return c.to_ascii_lowercase();
        }
        if let Some(p) = &self.path
            && let Some(ext) = std::path::Path::new(p).extension()
        {
            return ext.to_string_lossy().to_ascii_lowercase();
        }
        match self.media_type.as_deref() {
            Some("Audio") => "mka".into(),
            _ => "mkv".into(),
        }
    }

    pub fn title(&self) -> String {
        self.name.clone().unwrap_or_else(|| "untitled".into())
    }

    pub fn season_no(&self) -> i32 {
        if let Some(n) = self.parent_index_number {
            return n;
        }
        // 部分服务器在扁平列表里不返回 ParentIndexNumber，退而从季名解析
        if let Some(name) = &self.season_name {
            let digits: String = name.chars().filter(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<i32>() {
                return n;
            }
            if name.to_ascii_lowercase().contains("special") {
                return 0;
            }
        }
        // 再退一步：从服务器路径里的 Season 目录或 SxxExx 解析
        if let Some(path) = &self.path {
            if let Some((season, _)) = parse_season_episode(path) {
                return season;
            }
            if let Some(n) = parse_season_dir(path) {
                return n;
            }
        }
        0
    }

    pub fn episode_no(&self) -> i32 {
        if let Some(n) = self.index_number {
            return n;
        }
        self.path
            .as_deref()
            .and_then(parse_season_episode)
            .map(|(_, e)| e)
            .unwrap_or(0)
    }
}

/// 从路径文件名解析 `SxxEyy`。
fn parse_season_episode(path: &str) -> Option<(i32, i32)> {
    let file = path.rsplit(['/', '\\']).next()?;
    let bytes = file.as_bytes();
    let mut i = 0;
    while i + 4 < bytes.len() {
        if bytes[i] == b'S' || bytes[i] == b's' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && j < bytes.len() && (bytes[j] == b'E' || bytes[j] == b'e') {
                let mut k = j + 1;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                if k > j + 1 {
                    let s = file[i + 1..j].parse::<i32>().ok()?;
                    let e = file[j + 1..k].parse::<i32>().ok()?;
                    return Some((s, e));
                }
            }
        }
        i += 1;
    }
    None
}

/// 从路径中的 `Season 1` / `Season 01` 目录解析季号。
fn parse_season_dir(path: &str) -> Option<i32> {
    for seg in path.split(['/', '\\']) {
        let lower = seg.trim().to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("season") {
            let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<i32>() {
                return Some(n);
            }
        }
    }
    None
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaSource {
    pub id: Option<String>,
    pub name: Option<String>,
    pub path: Option<String>,
    pub container: Option<String>,
    pub size: Option<u64>,
    pub bitrate: Option<i64>,
    pub media_streams: Option<Vec<MediaStream>>,
    pub supports_direct_stream: Option<bool>,
    pub supports_direct_play: Option<bool>,
    pub is_remote: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaStream {
    pub index: Option<i32>,
    /// Video / Audio / Subtitle / EmbeddedImage
    #[serde(rename = "Type")]
    pub stream_type: Option<String>,
    pub codec: Option<String>,
    pub language: Option<String>,
    pub display_title: Option<String>,
    pub is_external: Option<bool>,
    pub is_default: Option<bool>,
    pub is_forced: Option<bool>,
    pub delivery_url: Option<String>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub channels: Option<i32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct QueryResult<T> {
    pub items: Vec<T>,
    pub total_record_count: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct User {
    pub id: String,
    pub name: Option<String>,
    pub is_administrator: Option<bool>,
    pub has_password: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
pub struct AuthResult {
    pub access_token: Option<String>,
    pub server_id: Option<String>,
    pub user: Option<User>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthRequest {
    #[serde(rename = "Username")]
    pub username: String,
    #[serde(rename = "Pw")]
    pub pw: String,
}

impl AuthRequest {
    pub fn new(username: impl Into<String>, pw: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            pw: pw.into(),
        }
    }
}

/// 播放/转码参数（`--transcode` 时才使用）。
#[derive(Debug, Clone, Default)]
pub struct TranscodeOpts {
    pub container: String,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub max_height: Option<u32>,
    pub video_bitrate: Option<u32>,
    pub audio_bitrate: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Probe {
    pub total: Option<u64>,
    pub range_supported: bool,
}
