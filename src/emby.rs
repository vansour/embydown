//! Emby Web API 客户端：认证、浏览、直链构造。

use anyhow::{bail, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, RETRY_AFTER};
use reqwest::Url;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::model::{AuthRequest, AuthResult, Item, MediaStream, QueryResult, User};
use crate::util::{vlog, verbose};

pub const CLIENT_NAME: &str = "embydown";

#[derive(Clone)]
pub struct Client {
    pub http: reqwest::Client,
    /// 规范化后的服务器根地址，无尾斜杠，例如 `http://192.168.1.10:8096`
    pub server: String,
    pub token: String,
    pub user_id: String,
    pub device_id: String,
    /// 是否跳过 TLS 证书校验（重建后台进程的 Client 时需要）
    pub insecure: bool,
}

/// 可序列化的连接参数：TUI 把下载任务交给后台守护进程时只传这些。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientCreds {
    pub server: String,
    pub token: String,
    pub user_id: String,
    pub insecure: bool,
}

impl Client {
    pub fn new(server: &str, insecure: bool) -> Result<Self> {
        let server = normalize_server(server)?;
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(insecure)
            .user_agent(format!("{}/{}", CLIENT_NAME, env!("CARGO_PKG_VERSION")))
            .pool_max_idle_per_host(64)
            .connect_timeout(Duration::from_secs(20))
            // 服务端接受连接后不再发送数据时，read_timeout 会在每次读取超时后报错，
            // 避免 probe/下载永久挂起（尤其是 TUI 里会永久占用并发槽位）。
            .read_timeout(Duration::from_secs(60))
            .build()
            .context("构建 HTTP 客户端失败")?;
        Ok(Self {
            http,
            server,
            token: String::new(),
            user_id: String::new(),
            device_id: format!("{}-cli", CLIENT_NAME),
            insecure,
        })
    }

    /// 供后台守护进程跨进程/重启恢复连接。
    pub fn creds(&self) -> ClientCreds {
        ClientCreds {
            server: self.server.clone(),
            token: self.token.clone(),
            user_id: self.user_id.clone(),
            insecure: self.insecure,
        }
    }

    pub fn from_creds(creds: &ClientCreds) -> Result<Self> {
        let mut client = Self::new(&creds.server, creds.insecure)?;
        client.token = creds.token.clone();
        client.user_id = creds.user_id.clone();
        Ok(client)
    }

    fn auth_header(&self) -> String {
        format!(
            "MediaBrowser Client=\"{}\", Device=\"CLI\", DeviceId=\"{}\", Version=\"{}\"",
            CLIENT_NAME,
            self.device_id,
            env!("CARGO_PKG_VERSION")
        )
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        h.insert(
            "X-Emby-Authorization",
            HeaderValue::from_str(&self.auth_header()).context("构造认证头失败")?,
        );
        h.insert(ACCEPT, HeaderValue::from_static("application/json"));
        if !self.token.is_empty() {
            h.insert(
                "X-Emby-Token",
                HeaderValue::from_str(&self.token).context("token 含非法字符")?,
            );
        }
        Ok(h)
    }

    /// 拼接 URL 并追加查询参数（自动带上 `api_key`）。
    pub fn build_url(&self, path: &str, query: &[(&str, String)]) -> String {
        let base = format!("{}{}", self.server, path);
        let mut url = match Url::parse(&base) {
            Ok(u) => u,
            Err(_) => return base,
        };
        {
            let mut qp = url.query_pairs_mut();
            if !self.token.is_empty() {
                qp.append_pair("api_key", &self.token);
            }
            for (k, v) in query {
                qp.append_pair(k, v);
            }
        }
        url.to_string()
    }

    /// 内部接口用的 URL（不含 api_key，鉴权走请求头）。
    fn api_url(&self, path: &str, query: &[(&str, String)]) -> String {
        let base = format!("{}{}", self.server, path);
        let mut url = match Url::parse(&base) {
            Ok(u) => u,
            Err(_) => return base,
        };
        if !query.is_empty() {
            let mut qp = url.query_pairs_mut();
            for (k, v) in query {
                qp.append_pair(k, v);
            }
        }
        url.to_string()
    }

    pub async fn login(&mut self, username: &str, password: &str) -> Result<()> {
        let url = self.api_url("/Users/AuthenticateByName", &[]);
        let body = AuthRequest::new(username, password);
        let resp = self
            .http
            .post(&url)
            .headers(self.headers()?)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("连接 {} 失败", self.server))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "登录失败：HTTP {}（请检查账号密码；Emby 部分实例会记录失败次数）\n{}",
                status.as_u16(),
                snippet(&text)
            );
        }
        let auth: AuthResult = serde_json::from_str(&text).context("解析登录响应失败")?;
        self.token = auth.access_token.context("登录响应里缺少 AccessToken")?;
        if let Some(u) = auth.user
            && !u.id.is_empty()
        {
            self.user_id = u.id;
        }
        vlog(1, format!("✅ 登录成功（用户 {}）", username));
        Ok(())
    }

    /// 用 API Key 时确定当前用户：先试 `/Users/Me`，再回退到用户列表。
    pub async fn resolve_user(&mut self) -> Result<()> {
        if !self.user_id.is_empty() {
            return Ok(());
        }
        if let Some(me) = self.try_get::<User>("/Users/Me", &[]).await
            && !me.id.is_empty()
        {
            self.user_id = me.id;
            vlog(2, format!("当前用户（/Users/Me）: {:?}", me.name));
            return Ok(());
        }
        let users: Vec<User> = self
            .get("/Users", &[])
            .await
            .context("获取用户列表失败（API Key 是否有效？）")?;
        let picked = users
            .iter()
            .find(|u| u.is_administrator.unwrap_or(false))
            .or_else(|| users.first())
            .context("服务器上没有可用用户")?;
        self.user_id = picked.id.clone();
        vlog(
            2,
            format!("当前用户（/Users 回退）: {:?} admin={:?}", picked.name, picked.is_administrator),
        );
        Ok(())
    }

    async fn try_get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Option<T> {
        let url = self.api_url(path, query);
        let resp = self.http.get(&url).headers(self.headers().ok()?).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<T>().await.ok()
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        let url = self.api_url(path, query);
        let mut attempt = 0u32;
        loop {
            let resp = self
                .http
                .get(&url)
                .headers(self.headers()?)
                .send()
                .await
                .with_context(|| format!("请求失败: {}", redact(&url)))?;
            let status = resp.status();
            let code = status.as_u16();
            let retry_after = resp
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            let text = resp.text().await.unwrap_or_default();
            if status.is_success() {
                return serde_json::from_str::<T>(&text)
                    .with_context(|| format!("解析响应失败 ← {}", redact(&url)));
            }
            // 服务器限流：退避重试，避免被临时封禁
            if matches!(code, 429 | 502 | 503 | 504) && attempt < 5 {
                let wait = retry_after.unwrap_or(1 << attempt).min(30);
                vlog(1, format!("  ↳ 服务器限流（HTTP {code}），{wait} 秒后重试"));
                tokio::time::sleep(Duration::from_secs(wait)).await;
                attempt += 1;
                continue;
            }
            bail!("HTTP {code} ← {}\n{}", redact(&url), snippet(&text));
        }
    }

    pub async fn item(&self, id: &str) -> Result<Item> {
        self.get(
            &format!("/Users/{}/Items/{}", self.user_id, id),
            &[("Fields", FIELDS.to_string())],
        )
        .await
        .with_context(|| format!("获取条目 {id} 失败（id 是否正确？）"))
    }

    /// 列出子项。`recursive=true` 会一直展开到叶子。
    ///
    /// 自动分页：部分服务器的单页上限很低（有的实例 Limit≥100 直接 404），
    /// 不指定 limit 时按 99 条一页拉全，指定 limit 时最多拉这么多条。
    pub async fn children(
        &self,
        parent_id: &str,
        recursive: bool,
        item_types: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<Item>> {
        let mut q: Vec<(&str, String)> = vec![
            ("ParentId", parent_id.to_string()),
            ("Recursive", recursive.to_string()),
            ("Fields", FIELDS.to_string()),
            ("SortBy", "SortName".to_string()),
            ("SortOrder", "Ascending".to_string()),
            ("ImageTypeLimit", "0".to_string()),
        ];
        if let Some(t) = item_types {
            q.push(("IncludeItemTypes", t.to_string()));
        }
        self.query_items(&format!("/Users/{}/Items", self.user_id), &q, limit)
            .await
            .with_context(|| format!("列出 {parent_id} 的子项失败"))
    }

    /// 按 `StartIndex` / `Limit` 自动分页拉取条目。
    async fn query_items(
        &self,
        path: &str,
        base: &[(&str, String)],
        limit: Option<u32>,
    ) -> Result<Vec<Item>> {
        const PAGE_SIZE: u32 = 99; // 有的实例 Limit≥100 会直接 404
        let cap = limit.unwrap_or(u32::MAX);
        if cap == 0 {
            return Ok(Vec::new());
        }

        let mut items: Vec<Item> = Vec::new();
        let mut start: u32 = 0;
        loop {
            let page = PAGE_SIZE.min(cap.saturating_sub(items.len() as u32));
            if page == 0 {
                break;
            }
            let mut q = base.to_vec();
            q.push(("StartIndex", start.to_string()));
            q.push(("Limit", page.to_string()));
            let res: QueryResult<Item> = self.get(path, &q).await?;
            let got = res.items.len() as u32;
            items.extend(res.items);
            start += got;
            let done = got == 0
                || items.len() as u32 >= cap
                || match res.total_record_count {
                    // 服务器可能把单页压得比请求的更小，以总数判断更稳
                    Some(t) => start as i64 >= t,
                    None => got < page,
                };
            if done {
                break;
            }
            // 温和限速，避免触发部分实例的防滥用策略
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        Ok(items)
    }

    pub async fn views(&self) -> Result<Vec<Item>> {
        let res: QueryResult<Item> = self
            .get(
                &format!("/Users/{}/Views", self.user_id),
                &[("Fields", FIELDS.to_string())],
            )
            .await
            .context("获取媒体库列表失败")?;
        Ok(res.items)
    }

    pub async fn search(&self, term: &str, limit: u32) -> Result<Vec<Item>> {
        let q: Vec<(&str, String)> = vec![
            ("SearchTerm", term.to_string()),
            ("Recursive", "true".to_string()),
            (
                "IncludeItemTypes",
                "Movie,Series,Episode,Video,Audio,MusicVideo".to_string(),
            ),
            ("Fields", FIELDS.to_string()),
        ];
        // 服务器返回顺序可能是按名称而不是相关度，直接截断会把真正相关的条目挤掉。
        // 先多取一些，在客户端按相关度排序后再截断到 limit。
        let fetch = limit.saturating_mul(4).min(400).max(limit);
        let mut items = self
            .query_items(&format!("/Users/{}/Items", self.user_id), &q, Some(fetch))
            .await
            .context("搜索失败")?;
        items.sort_by_cached_key(|item| {
            (
                std::cmp::Reverse(item.search_score(term)),
                item.title().chars().count(),
                item.title().to_lowercase(),
            )
        });
        items.truncate(limit as usize);
        Ok(items)
    }

    /// 直链候选列表：优先静态直链（对非管理员账号权限要求最低），再回退官方 Download。
    pub fn media_urls(&self, item: &Item) -> Vec<String> {
        let ms_id = item.media_source_id().unwrap_or_default().to_string();
        let mut common: Vec<(&str, String)> = vec![("static", "true".to_string())];
        if !ms_id.is_empty() {
            common.push(("MediaSourceId", ms_id));
        }

        let mut urls = Vec::new();
        match item.item_type.as_deref() {
            Some("Audio") => urls.push(self.build_url(&format!("/Audio/{}/stream", item.id), &common)),
            _ => {
                urls.push(self.build_url(&format!("/Videos/{}/stream", item.id), &common));
                urls.push(self.build_url(
                    &format!("/Videos/{}/stream.{}", item.id, item.ext()),
                    &common,
                ));
            }
        }
        // 回退接口（/Items/{id}/Download、/Items/{id}/File）无法指定媒体源，
        // 锁定某个版本后它们只会返回服务器的默认源。宁可报错，也不能把
        // “错误的文件”存成用户选中的版本。
        if !item.source_locked {
            // 回退 1：官方下载接口
            urls.push(self.build_url(&format!("/Items/{}/Download", item.id), &[]));
            // 回退 2：原文件接口（部分版本/权限下可用）
            urls.push(self.build_url(&format!("/Items/{}/File", item.id), &[]));
        }
        urls.dedup();
        urls
    }

    /// 外挂字幕候选 URL（先试 DeliveryUrl，再按 API 规则构造）。
    pub fn subtitle_urls(&self, item: &Item, stream: &MediaStream) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(d) = &stream.delivery_url {
            let joined = if d.starts_with("http://") || d.starts_with("https://") {
                d.clone()
            } else {
                format!("{}{}", self.server, if d.starts_with('/') { d.clone() } else { format!("/{}", d) })
            };
            out.push(self.build_url(
                joined.trim_start_matches(&self.server),
                &[],
            ));
        }
        let idx = stream.index.unwrap_or(0);
        // 用标准字幕后缀构造 URL：codec 常是 `subrip`，直接当扩展名会生成
        // 无法被播放器/扫描器识别的 `.subrip` 文件。
        let codec = crate::naming::subtitle_ext(stream.codec.as_deref());
        if let Some(ms) = item.media_source_id() {
            out.push(self.build_url(
                &format!("/Videos/{}/{}/Subtitles/{}/Stream.{}", item.id, ms, idx, codec),
                &[],
            ));
        }
        out.dedup();
        out
    }

}

const FIELDS: &str = "MediaSources,MediaStreams,Path,Overview,ProductionYear,ParentId,SeriesId,SeriesName,SeasonName,IndexNumber,ParentIndexNumber,RunTimeTicks,DateCreated,BasicSyncInfo";

/// 规范化服务器地址：补 scheme、去尾斜杠、去多余空白。
pub fn normalize_server(input: &str) -> Result<String> {
    let s = input.trim();
    if s.is_empty() {
        bail!("服务器地址为空");
    }
    let with_scheme = if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else {
        format!("http://{s}")
    };
    let mut url = Url::parse(&with_scheme).with_context(|| format!("服务器地址无法解析: {s}"))?;
    if url.path() == "/" || url.path().is_empty() {
        // 保持根路径
    }
    let mut out = with_scheme.trim_end_matches('/').to_string();
    if url.query().is_some() || url.fragment().is_some() {
        url.set_query(None);
        url.set_fragment(None);
        out = url.as_str().trim_end_matches('/').to_string();
    }
    Ok(out)
}

/// 日志里隐藏 api_key。
pub fn redact(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut u) => {
            let pairs: Vec<(String, String)> = u
                .query_pairs()
                .map(|(k, v)| {
                    if k.eq_ignore_ascii_case("api_key") || k.eq_ignore_ascii_case("token") {
                        (k.to_string(), "***".to_string())
                    } else {
                        (k.to_string(), v.to_string())
                    }
                })
                .collect();
            u.set_query(None);
            if !pairs.is_empty() {
                let mut qp = u.query_pairs_mut();
                for (k, v) in pairs {
                    qp.append_pair(&k, &v);
                }
            }
            u.to_string()
        }
        Err(_) => redact_text(url),
    }
}

/// 对任意文本（错误链、日志）脱敏。
///
/// `reqwest::Error` 的 Display 会带上完整请求 URL，直接打印 `{e:#}` 会把
/// `api_key` 泄漏到终端/CI 日志里；所有面向用户的错误输出都应先过这里。
pub fn redact_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    loop {
        let lower = rest.to_ascii_lowercase();
        let next = ["api_key=", "token="]
            .iter()
            .filter_map(|needle| lower.find(needle).map(|p| (p, needle.len())))
            .min_by_key(|(p, _)| *p);
        let Some((pos, len)) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..pos + len]);
        out.push_str("***");
        let after = &rest[pos + len..];
        let end = after
            .find(|c: char| {
                c == '&' || c == '#' || c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '>' | ']')
            })
            .unwrap_or(after.len());
        rest = &after[end..];
    }
}

fn snippet(text: &str) -> String {
    let t = text.trim();
    let max = if verbose() > 0 { 800 } else { 300 };
    if t.chars().count() <= max {
        t.to_string()
    } else {
        format!("{}…", t.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MediaSource;

    fn item(locked: bool) -> Item {
        Item {
            id: "v1".into(),
            item_type: Some("Movie".into()),
            container: Some("mkv".into()),
            media_sources: Some(vec![MediaSource {
                id: Some("ms1".into()),
                ..Default::default()
            }]),
            version: locked.then(|| "1080p".to_string()),
            source_locked: locked,
            ..Default::default()
        }
    }

    #[test]
    fn redact_text_masks_query_credentials() {
        let text =
            "error sending request for url (http://h/Videos/1/stream?static=true&api_key=SECRET)";
        let out = redact_text(text);
        assert!(!out.contains("SECRET"), "{out}");
        assert!(out.contains("api_key=***"), "{out}");
        assert!(
            redact_text("X-Emby-Token=abcd&x=1")
                .to_ascii_lowercase()
                .contains("token=***")
        );
    }

    #[test]
    fn version_blind_fallbacks_are_omitted_when_a_source_is_locked() {
        let client = Client::new("http://127.0.0.1:8096", false).unwrap();

        let urls = client.media_urls(&item(false));
        assert!(urls.iter().any(|u| u.contains("/Items/v1/Download")));
        assert!(urls.iter().any(|u| u.contains("/Items/v1/File")));
        assert!(
            urls.iter()
                .all(|u| u.contains("/Items/v1/") || u.contains("MediaSourceId=ms1"))
        );

        let urls = client.media_urls(&item(true));
        assert!(!urls.iter().any(|u| u.contains("/Items/v1/Download")));
        assert!(!urls.iter().any(|u| u.contains("/Items/v1/File")));
        assert!(urls.iter().all(|u| u.contains("MediaSourceId=ms1")));
    }
}
