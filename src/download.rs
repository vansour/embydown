//! 直链下载器：Range 探测 → 分块并发 → 断点续传 → 落盘重命名。
//!
//! 设计要点：
//! 1. 默认走静态直链（`/Videos/{id}/stream?static=true`），对账号权限要求最低；
//!    失败自动回退 `/Items/{id}/Download`（官方下载接口）。
//! 2. 先用 `Range: bytes=0-0` 探测总大小与是否支持分段，支持则按 `--connections` 并发拉块，
//!    不支持就退化成单连接流式下载。
//! 3. 每个块写完即记入 `<file>.part.json`，中断后重跑同一条命令可续传。

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, Semaphore};

use crate::emby::{self, Client};
use crate::model::{Item, Probe, TranscodeOpts};
use crate::naming;
use crate::util::{format_bytes, sanitize_component, vlog};

/// 小于这个大小就不分块了（分块开销不值）。
const MIN_CHUNK: u64 = 4 * 1024 * 1024;
const MAX_RETRY: u32 = 3;

#[derive(Debug, Clone)]
pub struct DownloadOpts {
    pub out: PathBuf,
    pub pattern: Option<String>,
    pub connections: usize,
    pub jobs: usize,
    pub limit_rate: Option<u64>,
    pub resume: bool,
    pub overwrite: bool,
    pub subs: bool,
    pub dry_run: bool,
    pub transcode: Option<TranscodeOpts>,
}

impl Default for DownloadOpts {
    fn default() -> Self {
        Self {
            out: PathBuf::from("./downloads"),
            pattern: None,
            connections: 8,
            jobs: 2,
            limit_rate: None,
            resume: true,
            overwrite: false,
            subs: true,
            dry_run: false,
            transcode: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Stats {
    pub ok: usize,
    pub skipped: usize,
    pub failed: usize,
    pub bytes: u64,
    pub subs: usize,
}

// ───────────────────────── 取消标记 ─────────────────────────

/// 用户主动中断（Ctrl+C）。用于把「取消」和真正的下载失败区分开。
#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "已取消")
    }
}

impl std::error::Error for Cancelled {}

// ───────────────────────── 限速器 ─────────────────────────

struct Throttle {
    start: Instant,
    bytes: u64,
}

pub struct Limiter {
    rate: Option<u64>,
    inner: Mutex<Throttle>,
}

impl Limiter {
    pub fn new(rate: Option<u64>) -> Self {
        Self {
            rate,
            inner: Mutex::new(Throttle {
                start: Instant::now(),
                bytes: 0,
            }),
        }
    }

    async fn acquire(&self, n: u64) {
        let wait = self.wait_for(n).await;
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// 限速等待，期间响应取消；返回 false 表示已取消。
    async fn acquire_cancellable(&self, n: u64, cancel: &AtomicBool) -> bool {
        let wait = self.wait_for(n).await;
        if wait.is_zero() {
            return !cancel.load(Ordering::SeqCst);
        }
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return false;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return true;
            }
            let step = (deadline - now).min(Duration::from_millis(100));
            tokio::time::sleep(step).await;
        }
    }

    async fn wait_for(&self, n: u64) -> Duration {
        let Some(rate) = self.rate else {
            return Duration::ZERO;
        };
        let mut g = self.inner.lock().await;
        g.bytes += n;
        let elapsed = g.start.elapsed().as_secs_f64();
        let expected = g.bytes as f64 / rate as f64;
        if expected > elapsed {
            Duration::from_secs_f64(expected - elapsed)
        } else {
            if elapsed > 10.0 {
                g.start = Instant::now();
                g.bytes = 0;
            }
            Duration::ZERO
        }
    }
}

// ───────────────────────── 断点状态 ─────────────────────────

#[derive(Debug, Serialize, Deserialize, Default)]
struct PartState {
    total: u64,
    chunk_size: u64,
    completed: Vec<u32>,
}

fn part_path(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.part", dest.display()))
}

fn state_path(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.part.json", dest.display()))
}

// ───────────────────────── 探测 ─────────────────────────

/// 探测直链：拿总大小 + 是否支持 Range。401/403 会给出明确提示。
pub async fn probe(http: &reqwest::Client, url: &str) -> Result<Probe> {
    let resp = http
        .get(url)
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .with_context(|| format!("连接直链失败: {}", emby::redact(url)))?;
    let status = resp.status();
    match status.as_u16() {
        206 => {
            let total = resp
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit('/').next())
                .and_then(|v| v.trim().parse::<u64>().ok());
            Ok(Probe {
                total,
                range_supported: total.is_some(),
            })
        }
        200 => {
            let len = resp
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            let accepts = resp
                .headers()
                .get(ACCEPT_RANGES)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.eq_ignore_ascii_case("bytes"))
                .unwrap_or(false);
            vlog(
                2,
                format!("  ↳ 服务器忽略 Range（Accept-Ranges={accepts}），退化为单连接"),
            );
            // 明明请求了 bytes=0-0 却回 200 整包 → 视为不支持分段
            Ok(Probe {
                total: len,
                range_supported: false,
            })
        }
        401 => bail!("HTTP 401 未授权：Token 失效或没有该媒体的权限"),
        403 => bail!("HTTP 403 被拒绝：该账号可能未开启「允许下载」/「允许播放」权限"),
        404 => bail!("HTTP 404：条目不存在，或该类型的直链路径不可用"),
        416 => bail!("HTTP 416：Range 越界（文件可能已被替换，删掉 .part 重试）"),
        other => bail!("HTTP {other}：直链不可用"),
    }
}

// ───────────────────────── 单文件下载 ─────────────────────────

/// 下载一个直链到 `dest`。返回写入字节数。
async fn download_file(
    http: &reqwest::Client,
    candidates: &[String],
    dest: &Path,
    pb: &ProgressBar,
    opts: &DownloadOpts,
    limiter: &Arc<Limiter>,
    cancel: &Arc<AtomicBool>,
) -> Result<u64> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }

    // 依次探测候选直链，第一个能用的就开工
    let mut probe_result: Option<Probe> = None;
    let mut used_url = String::new();
    let mut last_err: Option<anyhow::Error> = None;
    for url in candidates {
        match probe(http, url).await {
            Ok(p) => {
                vlog(2, format!("  ↳ 直链可用: {}", emby::redact(url)));
                used_url = url.clone();
                probe_result = Some(p);
                break;
            }
            Err(e) => {
                vlog(2, format!("  ↳ 直链不可用: {} ({})", emby::redact(url), e));
                last_err = Some(e);
            }
        }
    }
    let probe = probe_result.ok_or_else(|| {
        last_err.unwrap_or_else(|| anyhow!("没有任何可用的直链"))
    })?;

    let total = probe.total;
    pb.set_length(total.unwrap_or(0));

    let part = part_path(dest);
    let state_file = state_path(dest);

    let can_chunk = opts.connections > 1
        && probe.range_supported
        && total.map(|t| t > MIN_CHUNK * 2).unwrap_or(false);

    let written = if can_chunk {
        let total = total.unwrap();
        download_chunked(
            http,
            &used_url,
            &part,
            &state_file,
            total,
            pb,
            opts,
            limiter,
            cancel,
        )
        .await?
    } else {
        download_single(
            http,
            &used_url,
            &part,
            total,
            pb,
            limiter,
            cancel.as_ref(),
            opts.resume,
            probe.range_supported,
        )
        .await?
    };

    if let Some(t) = total {
        let meta = tokio::fs::metadata(&part).await.context("读取临时文件失败")?;
        if meta.len() != t {
            bail!(
                "文件大小不符：期望 {} 字节，实际 {} 字节（保留 .part 便于续传）",
                t,
                meta.len()
            );
        }
    }

    if dest.exists() && opts.overwrite {
        tokio::fs::remove_file(dest).await.ok();
    }
    tokio::fs::rename(&part, dest)
        .await
        .with_context(|| format!("重命名 {} → {} 失败", part.display(), dest.display()))?;
    tokio::fs::remove_file(&state_file).await.ok();

    Ok(written)
}

/// 分块并发下载。
#[allow(clippy::too_many_arguments)]
async fn download_chunked(
    http: &reqwest::Client,
    url: &str,
    part: &Path,
    state_file: &Path,
    total: u64,
    pb: &ProgressBar,
    opts: &DownloadOpts,
    limiter: &Arc<Limiter>,
    cancel: &Arc<AtomicBool>,
) -> Result<u64> {
    let target_chunks = (opts.connections as u64 * 4).max(4);
    let chunk_size = total.div_ceil(target_chunks).max(MIN_CHUNK);
    let count = total.div_ceil(chunk_size);

    // 续传：状态文件与 part 都对得上才复用
    let mut completed: HashSet<u32> = HashSet::new();
    if opts.resume
        && part.exists()
        && let Ok(text) = tokio::fs::read_to_string(state_file).await
        && let Ok(st) = serde_json::from_str::<PartState>(&text)
        && st.total == total
        && st.chunk_size == chunk_size
    {
        completed = st.completed.into_iter().filter(|i| *i < count as u32).collect();
        vlog(
            1,
            format!(
                "  ↳ 续传：已有 {}/{} 块（{}）",
                completed.len(),
                count,
                format_bytes(chunk_size * completed.len() as u64)
            ),
        );
    }

    // 预分配并保留已下载内容
    {
        let f = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(part)
            .await
            .with_context(|| format!("打开临时文件失败: {}", part.display()))?;
        f.set_len(total).await.context("预分配文件大小失败")?;
    }

    pb.set_position(completed.iter().map(|_| chunk_size).sum::<u64>().min(total));
    let done = Arc::new(Mutex::new(completed));
    let sem = Arc::new(Semaphore::new(opts.connections.max(1)));
    let mut set = tokio::task::JoinSet::new();
    let mut errors: Vec<String> = Vec::new();
    let mut cancelled = false;

    for idx in 0..count {
        {
            let g = done.lock().await;
            if g.contains(&(idx as u32)) {
                continue;
            }
        }
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        let start = idx * chunk_size;
        let end = (start + chunk_size).min(total) - 1;
        let permit = sem.clone().acquire_owned().await.context("并发控制失败")?;
        let http = http.clone();
        let url = url.to_string();
        let part = part.to_path_buf();
        let pb = pb.clone();
        let done = done.clone();
        let state_file = state_file.to_path_buf();
        let chunk_size_copy = chunk_size;
        let total_copy = total;
        let limiter = limiter.clone();
        let cancel = Arc::clone(cancel);

        set.spawn(async move {
            let _permit = permit;
            fetch_chunk(&http, &url, start, end, &part, &pb, &limiter, &cancel).await?;
            let mut g = done.lock().await;
            g.insert(idx as u32);
            // 记状态（体积很小，直接覆盖写）
            let st = PartState {
                total: total_copy,
                chunk_size: chunk_size_copy,
                completed: g.iter().copied().collect(),
            };
            if let Ok(json) = serde_json::to_vec(&st) {
                let _ = tokio::fs::write(&state_file, json).await;
            }
            Ok::<(), anyhow::Error>(())
        });
    }

    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if e.downcast_ref::<Cancelled>().is_some() {
                    cancelled = true;
                } else {
                    errors.push(e.to_string());
                }
            }
            Err(e) => errors.push(format!("任务崩溃: {e}")),
        }
    }

    if cancelled {
        return Err(anyhow::Error::new(Cancelled));
    }
    if !errors.is_empty() {
        bail!(
            "{} 个分块下载失败（.part 与状态已保留，可直接重试续传）：{}",
            errors.len(),
            errors.join(" | ")
        );
    }

    let finished: u64 = done.lock().await.iter().map(|_| chunk_size).sum::<u64>().min(total);
    // 进度条按实际写入量校正
    pb.set_position(finished);
    Ok(total)
}

/// 下载单个块（带退避重试，失败回退进度）。
#[allow(clippy::too_many_arguments)]
async fn fetch_chunk(
    http: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
    part: &Path,
    pb: &ProgressBar,
    limiter: &Limiter,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..MAX_RETRY {
        if cancel.load(Ordering::SeqCst) {
            return Err(anyhow::Error::new(Cancelled));
        }
        match fetch_chunk_once(http, url, start, end, part, pb, limiter, cancel).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if e.downcast_ref::<Cancelled>().is_some() {
                    return Err(e);
                }
                vlog(
                    1,
                    format!(
                        "  ↳ 块 {}-{} 第 {} 次失败: {}",
                        start,
                        end,
                        attempt + 1,
                        e
                    ),
                );
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(400u64 << attempt)).await;
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("未知错误")))
}

#[allow(clippy::too_many_arguments)]
async fn fetch_chunk_once(
    http: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
    part: &Path,
    pb: &ProgressBar,
    limiter: &Limiter,
    cancel: &AtomicBool,
) -> Result<()> {
    let resp = http
        .get(url)
        .header(RANGE, format!("bytes={start}-{end}"))
        .send()
        .await?;
    if resp.status().as_u16() != 206 {
        bail!("期望 206，实际 HTTP {}", resp.status().as_u16());
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(part)
        .await
        .context("打开临时文件失败")?;
    file.seek(std::io::SeekFrom::Start(start)).await?;

    let mut written: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::SeqCst) {
            file.flush().await.ok();
            return Err(anyhow::Error::new(Cancelled));
        }
        match chunk {
            Ok(bytes) => {
                file.write_all(&bytes).await.context("写盘失败（磁盘满？）")?;
                written += bytes.len() as u64;
                pb.inc(bytes.len() as u64);
                if !limiter
                    .acquire_cancellable(bytes.len() as u64, cancel)
                    .await
                {
                    return Err(anyhow::Error::new(Cancelled));
                }
            }
            Err(e) => {
                pb.dec(written);
                return Err(anyhow!("传输中断: {e}"));
            }
        }
    }
    file.flush().await.ok();
    let expected = end - start + 1;
    if written != expected {
        pb.dec(written);
        bail!("块长度不符：期望 {expected}，实际 {written}");
    }
    Ok(())
}

/// 单连接流式下载（服务器不支持分段或只开了 1 个连接）。
///
/// 若 `.part` 已有内容且服务器支持 Range，则从断点续写。
#[allow(clippy::too_many_arguments)]
async fn download_single(
    http: &reqwest::Client,
    url: &str,
    part: &Path,
    total: Option<u64>,
    pb: &ProgressBar,
    limiter: &Limiter,
    cancel: &AtomicBool,
    resume: bool,
    range_ok: bool,
) -> Result<u64> {
    let existing = if resume {
        tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    let can_resume = existing > 0
        && range_ok
        && total.map(|t| existing < t).unwrap_or(false);
    let start = if can_resume { existing } else { 0 };

    let mut req = http.get(url);
    if start > 0 {
        req = req.header(RANGE, format!("bytes={start}-"));
    }
    let resp = req.send().await.context("建立下载连接失败")?;
    let code = resp.status().as_u16();
    if start > 0 && code != 206 {
        // 服务器没按 Range 回包，只能重头来
        vlog(1, format!("  ↳ 服务器未按 Range 响应（HTTP {code}），从头下载"));
        return Box::pin(download_single(
            http, url, part, total, pb, limiter, cancel, false, false,
        ))
        .await;
    }
    if code != 200 && code != 206 {
        bail!("下载失败：HTTP {code}");
    }
    if start > 0 {
        vlog(1, format!("  ↳ 单连接续传：从 {} 继续", crate::util::format_bytes(start)));
        pb.set_position(start);
    }

    let mut file = if start > 0 {
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(part)
            .await
            .with_context(|| format!("打开临时文件失败: {}", part.display()))?
    } else {
        tokio::fs::File::create(part)
            .await
            .with_context(|| format!("创建临时文件失败: {}", part.display()))?
    };

    if let Some(t) = total.or_else(|| resp.content_length().map(|c| c + start)) {
        pb.set_length(t);
    }

    let mut written: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::SeqCst) {
            file.flush().await.ok();
            return Err(anyhow::Error::new(Cancelled));
        }
        let bytes = chunk.context("传输中断")?;
        file.write_all(&bytes).await.context("写盘失败（磁盘满？）")?;
        written += bytes.len() as u64;
        pb.inc(bytes.len() as u64);
        if !limiter
            .acquire_cancellable(bytes.len() as u64, cancel)
            .await
        {
            file.flush().await.ok();
            return Err(anyhow::Error::new(Cancelled));
        }
    }
    file.flush().await.ok();
    Ok(start + written)
}

// ───────────────────────── 字体/字幕 ─────────────────────────

/// 下载外挂字幕（best-effort：找不到就跳过，不影响正片）。
async fn download_subs(
    client: &Client,
    item: &Item,
    video_dest: &Path,
    video_rel: &str,
    limiter: &Limiter,
    cancel: &AtomicBool,
    overwrite: bool,
) -> usize {
    let streams: Vec<&crate::model::MediaStream> = item
        .media_streams
        .iter()
        .flatten()
        .chain(
            item.media_sources
                .iter()
                .flatten()
                .flat_map(|ms| ms.media_streams.iter().flatten()),
        )
        .filter(|s| {
            s.stream_type.as_deref() == Some("Subtitle") && s.is_external.unwrap_or(false)
        })
        .collect();

    if streams.is_empty() {
        return 0;
    }
    let http = client.http.clone();
    let mut ok = 0usize;
    for s in streams {
        let lang = s.language.clone().unwrap_or_else(|| "und".into());
        let ext = s.codec.clone().unwrap_or_else(|| "srt".into());
        let forced = s.is_forced.unwrap_or(false);
        let rel = naming::subtitle_path(video_rel, &lang, &ext, forced);
        let file_name = Path::new(&rel)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("sub.{lang}.{ext}"));
        // 字幕与视频同目录、同基名
        let target = video_dest.with_file_name(sanitize_component(&file_name));
        if target.exists() && !overwrite {
            continue;
        }
        let urls = client.subtitle_urls(item, s);
        let mut saved = false;
        for url in urls {
            match fetch_small(&http, &url, &target).await {
                Ok(true) => {
                    ok += 1;
                    saved = true;
                    vlog(1, format!("  ↳ 字幕已保存: {}", target.display()));
                    break;
                }
                Ok(false) => continue,
                Err(e) => vlog(2, format!("  ↳ 字幕下载失败: {e}")),
            }
            limiter.acquire(1024).await;
        }
        if !saved {
            vlog(2, format!("  ↳ 跳过字幕 {lang}（服务器未提供可下载的外挂字幕）"));
        }
        if cancel.load(Ordering::SeqCst) {
            break;
        }
    }
    ok
}

/// 小文件下载：返回 true 表示保存成功。
async fn fetch_small(http: &reqwest::Client, url: &str, dest: &Path) -> Result<bool> {
    let resp = http.get(url).send().await?;
    let status = resp.status();
    if status.as_u16() != 200 && status.as_u16() != 206 {
        return Ok(false);
    }
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    // 服务器报错常返回 HTML，别把 404 页面存成字幕
    if ctype.contains("text/html") {
        return Ok(false);
    }
    let bytes = resp.bytes().await?;
    if bytes.is_empty() || bytes.len() > 32 * 1024 * 1024 {
        return Ok(false);
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::write(dest, &bytes).await?;
    Ok(true)
}

// ───────────────────────── 编排 ─────────────────────────

fn style_template() -> &'static str {
    if crate::util::verbose() >= 2 {
        "{spinner:.green} {msg} [{bar:28.cyan/blue}] {bytes}/{total_bytes} ({percent}%) {bytes_per_sec} ETA {eta}"
    } else {
        "{spinner:.green} {msg} [{bar:28.cyan/blue}] {bytes}/{total_bytes} {bytes_per_sec}"
    }
}

fn make_bar(mp: &MultiProgress, total: u64) -> ProgressBar {
    let pb = mp.add(ProgressBar::new(total));
    pb.set_style(
        ProgressStyle::with_template(style_template())
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=>-"),
    );
    pb
}

/// 批量下载入口。
pub async fn execute(client: &Client, items: Vec<Item>, opts: DownloadOpts) -> Result<Stats> {
    let mp = MultiProgress::new();
    let cancel = Arc::new(AtomicBool::new(false));
    let limiter = Arc::new(Limiter::new(opts.limit_rate));
    let sem = Arc::new(Semaphore::new(opts.jobs.max(1)));
    let fail_streak = Arc::new(AtomicUsize::new(0));
    let mut stats = Stats::default();
    let mut set: tokio::task::JoinSet<TaskResult> = tokio::task::JoinSet::new();

    // Ctrl-C：置取消位，已完成的块会被记录，重跑即续传
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\n⚠️  收到中断，正在收尾（已完成分块已记录，重跑同一命令可续传）…");
                cancel.store(true, Ordering::SeqCst);
            }
        });
    }

    let total_items = items.len();
    vlog(
        1,
        format!(
            "开始下载 {} 个条目 → {}（每文件 {} 连接，{} 个文件并发）",
            total_items,
            opts.out.display(),
            opts.connections,
            opts.jobs
        ),
    );

    for (idx, item) in items.into_iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        if fail_streak.load(Ordering::SeqCst) >= 3 {
            eprintln!("⚠️  连续 3 个条目失败，停止派发（先用 --verbose 看具体错误）");
            break;
        }
        let permit = sem.clone().acquire_owned().await.context("并发控制失败")?;
        let client = client.clone();
        let opts = opts.clone();
        let limiter = limiter.clone();
        let cancel = cancel.clone();
        let fail_streak = fail_streak.clone();
        let mp2 = mp.clone();

        set.spawn(async move {
            let _permit = permit;
            let rel = naming::render(
                opts.pattern
                    .as_deref()
                    .unwrap_or_else(|| naming::default_pattern(&item)),
                &item,
                &item.ext(),
            );
            let dest = opts.out.join(&rel);
            let label = format!(
                "[{}/{}] {}",
                idx + 1,
                total_items,
                shorten(&rel, 56)
            );

            if opts.dry_run {
                let urls = match &item.direct_url {
                    Some(u) => vec![u.clone()],
                    None => client.media_urls(&item, opts.transcode.as_ref()),
                };
                println!("📄 {rel}");
                println!("   → {}", dest.display());
                for u in urls {
                    println!("   ↳ {}", emby::redact(&u));
                }
                return TaskResult::Skipped;
            }

            // 已完整存在则跳过
            if dest.exists() && !opts.overwrite {
                let existing = tokio::fs::metadata(&dest).await.map(|m| m.len()).ok();
                if let Some(size) = item.size() {
                    if existing == Some(size) {
                        println!("⏭  已存在，跳过: {rel}");
                        return TaskResult::Skipped;
                    }
                } else {
                    println!("⏭  已存在，跳过: {rel}（加 --overwrite 覆盖）");
                    return TaskResult::Skipped;
                }
            }

            let pb = make_bar(&mp2, 0);
            pb.set_message(shorten(&label, 56));
            let urls = match &item.direct_url {
                Some(u) => vec![u.clone()],
                None => client.media_urls(&item, opts.transcode.as_ref()),
            };

            let started = Instant::now();
            let result = download_file(
                &client.http,
                &urls,
                &dest,
                &pb,
                &opts,
                &limiter,
                &cancel,
            )
            .await;

            match result {
                Ok(bytes) => {
                    pb.finish_and_clear();
                    let secs = started.elapsed().as_secs_f64().max(0.001);
                    println!(
                        "✅ {}  ({}, {:.1} MiB/s)",
                        dest.display(),
                        format_bytes(bytes),
                        bytes as f64 / 1024.0 / 1024.0 / secs
                    );
                    fail_streak.store(0, Ordering::SeqCst);
                    let subs = if opts.subs {
                        download_subs(
                            &client,
                            &item,
                            &dest,
                            &rel,
                            limiter.as_ref(),
                            cancel.as_ref(),
                            opts.overwrite,
                        )
                        .await
                    } else {
                        0
                    };
                    TaskResult::Ok { bytes, subs }
                }
                Err(e) if e.downcast_ref::<Cancelled>().is_some() => {
                    pb.finish_and_clear();
                    TaskResult::Cancelled
                }
                Err(e) => {
                    pb.finish_and_clear();
                    eprintln!("❌ {rel}\n   {e:#}");
                    fail_streak.fetch_add(1, Ordering::SeqCst);
                    TaskResult::Failed
                }
            }
        });
    }

    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(TaskResult::Ok { bytes, subs }) => {
                stats.ok += 1;
                stats.bytes += bytes;
                stats.subs += subs;
            }
            Ok(TaskResult::Skipped) => stats.skipped += 1,
            Ok(TaskResult::Cancelled) => stats.skipped += 1,
            Ok(TaskResult::Failed) => stats.failed += 1,
            Err(e) => {
                stats.failed += 1;
                eprintln!("❌ 任务异常: {e}");
            }
        }
    }

    println!(
        "\n完成：成功 {}，跳过 {}，失败 {}，共 {} 字节{}",
        stats.ok,
        stats.skipped,
        stats.failed,
        stats.bytes,
        if stats.subs > 0 {
            format!("，字幕 {} 个", stats.subs)
        } else {
            String::new()
        }
    );
    if stats.failed > 0 {
        bail!("有 {} 个条目未完成", stats.failed);
    }
    Ok(stats)
}

/// 单个条目的下载结果（供后台队列使用）。
#[derive(Debug, Default, Clone, Copy)]
pub struct OneResult {
    pub bytes: u64,
    pub subs: usize,
    pub skipped: bool,
}

/// 是否因为用户取消而结束。
pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Cancelled>().is_some()
}

/// 下载单个条目：不打印任何内容，进度写入 `pb`（可以是隐藏进度条）。
///
/// 与 CLI 的 `execute` 共用探测、分块、续传、字幕逻辑。
pub async fn download_one(
    client: &Client,
    item: &Item,
    opts: &DownloadOpts,
    limiter: &Arc<Limiter>,
    cancel: &Arc<AtomicBool>,
    pb: &ProgressBar,
) -> Result<OneResult> {
    let rel = naming::render(
        opts.pattern
            .as_deref()
            .unwrap_or_else(|| naming::default_pattern(item)),
        item,
        &item.ext(),
    );
    let dest = opts.out.join(&rel);

    // 已完整存在则跳过
    if dest.exists() && !opts.overwrite {
        let existing = tokio::fs::metadata(&dest).await.map(|m| m.len()).ok();
        if let Some(size) = item.size() {
            if existing == Some(size) {
                return Ok(OneResult {
                    skipped: true,
                    ..Default::default()
                });
            }
        } else {
            return Ok(OneResult {
                skipped: true,
                ..Default::default()
            });
        }
    }

    let urls = match &item.direct_url {
        Some(u) => vec![u.clone()],
        None => client.media_urls(item, opts.transcode.as_ref()),
    };
    pb.set_message(item.title());
    let bytes = download_file(&client.http, &urls, &dest, pb, opts, limiter, cancel).await?;
    let subs = if opts.subs {
        download_subs(client, item, &dest, &rel, limiter.as_ref(), cancel, opts.overwrite).await
    } else {
        0
    };
    Ok(OneResult {
        bytes,
        subs,
        skipped: false,
    })
}

enum TaskResult {
    Ok { bytes: u64, subs: usize },
    Skipped,
    Cancelled,
    Failed,
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("…{}", s.chars().skip(s.chars().count() - max + 1).collect::<String>())
    }
}
