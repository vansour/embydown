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
use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, RANGE, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, OwnedMutexGuard, Semaphore};

use crate::emby::{self, Client};
use crate::model::{Item, Probe};
use crate::naming;
use crate::util::{format_bytes, sanitize_component, vlog};

/// 小于这个大小就不分块了（分块开销不值）。
const MIN_CHUNK: u64 = 4 * 1024 * 1024;
/// 单个分块上限：超大文件不按比例切成巨型块，否则一块失败要重下很久。
const MAX_CHUNK: u64 = 512 * 1024 * 1024;
const MAX_RETRY: u32 = 3;

/// 分块数量与并发数解耦：默认把文件切成约 8 块。
///
/// 以前的块大小按 `connections * 4` 计算，用户为了避开服务器限流把
/// `-c` 调小后，已有的 `.part.json` 会因块布局不同而作废，几 GB 的
/// 进度被静默丢弃。现在布局只由文件大小决定，`-c` 只控制并发；
/// 块数也刻意保持较少，减少触发 429 的请求量。
const DEFAULT_TARGET_CHUNKS: u64 = 8;

/// 默认块大小：只由文件大小决定，与 `-c` 无关。
fn default_chunk_size(total: u64) -> u64 {
    total.div_ceil(DEFAULT_TARGET_CHUNKS).clamp(MIN_CHUNK, MAX_CHUNK)
}

/// 429 自动等待上限。超过这个时长就中止本次任务，避免进程挂起一小时。
const MAX_AUTO_RATE_WAIT: Duration = Duration::from_secs(300);

/// 限流响应没带 Retry-After 时的默认等待。
const DEFAULT_RATE_WAIT: Duration = Duration::from_secs(30);

/// 连续遇到限流时最多重试几次（每次都会按 Retry-After 等待）。
const MAX_RATE_RETRIES: u32 = 5;

/// 把 reqwest 错误转成脱敏后的 anyhow 错误。
///
/// `reqwest::Error` 的 Display 会带完整 URL（含 `api_key`），直接进 anyhow
/// 错误链后 `{e:#}` 会把 token 打到终端和 CI 日志里。
fn net_error(context: &str, e: reqwest::Error) -> anyhow::Error {
    anyhow!("{}: {}", context, emby::redact_text(&e.to_string()))
}

// ───────────────────────── 限流（HTTP 429） ─────────────────────────

/// 服务器返回 429 时抛出的错误，携带 `Retry-After` 解析出的等待时长。
#[derive(Debug)]
struct RateLimited {
    retry_after: Option<Duration>,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.retry_after {
            Some(d) if !d.is_zero() => write!(
                f,
                "服务器限流（HTTP 429），预计 {} 后解除；已下载的分块已保留，稍后重跑可续传",
                format_wait(d)
            ),
            _ => write!(
                f,
                "服务器限流（HTTP 429）；已下载的分块已保留，稍后重跑可续传"
            ),
        }
    }
}

impl std::error::Error for RateLimited {}

fn format_wait(d: Duration) -> String {
    let secs = d.as_secs().max(1);
    if secs >= 3600 {
        format!("{} 小时 {} 分", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{} 分 {} 秒", secs / 60, secs % 60)
    } else {
        format!("{secs} 秒")
    }
}

/// 解析 `Retry-After`（秒数形式）。HTTP-date 形式少见，解析不了就按默认值退避。
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    raw.parse::<u64>()
        .ok()
        .map(|s| Duration::from_secs(s.min(24 * 60 * 60)))
}

/// 每个主机一份限流状态：一个服务器被限流时不应阻塞另一个服务器。
static RATE_LIMITS: OnceLock<std::sync::Mutex<HashMap<String, Arc<RateLimitState>>>> =
    OnceLock::new();

#[derive(Default)]
struct RateLimitState {
    /// 在此时刻之前不再发送请求。
    until: std::sync::Mutex<Option<Instant>>,
    /// 暂停时间过长时置位：窗口内直接失败，而不是挂起等待。
    fatal: AtomicBool,
}

/// 获取某个 URL 所属主机的限流状态。
fn rate_limit_for(url: &str) -> Arc<RateLimitState> {
    let key = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_else(|| url.to_string());
    let map = RATE_LIMITS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(key)
        .or_insert_with(|| Arc::new(RateLimitState::default()))
        .clone()
}

impl RateLimitState {
    fn remaining(&self) -> Duration {
        let g = self.until.lock().unwrap_or_else(|e| e.into_inner());
        g.and_then(|t| t.checked_duration_since(Instant::now()))
            .unwrap_or_default()
    }

    /// 记录一次 429；返回 true 表示暂停窗口被延长（调用方据此只提示一次）。
    fn note(&self, wait: Duration) -> bool {
        let wait = wait.max(Duration::from_secs(1));
        let remaining = self.remaining();
        if remaining.is_zero() {
            self.fatal
                .store(wait > MAX_AUTO_RATE_WAIT, Ordering::SeqCst);
        } else if wait > MAX_AUTO_RATE_WAIT {
            self.fatal.store(true, Ordering::SeqCst);
        }
        let until = Instant::now() + wait;
        let mut g = self.until.lock().unwrap_or_else(|e| e.into_inner());
        let extended = g.is_none_or(|t| t < until);
        if extended {
            *g = Some(until);
        }
        extended
    }

    /// 当前是否处于“等待太久、直接失败”的限流窗口。
    fn is_blocking(&self) -> bool {
        self.fatal.load(Ordering::SeqCst) && !self.remaining().is_zero()
    }

    /// 发送请求前调用：短限流在这里睡眠等待，长限流立即返回 `Limited`。
    async fn enter(&self, cancel: &AtomicBool) -> Gate {
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Gate::Cancelled;
            }
            let remain = self.remaining();
            if remain.is_zero() {
                return Gate::Ready;
            }
            if self.fatal.load(Ordering::SeqCst) {
                return Gate::Limited;
            }
            let step = remain.min(Duration::from_millis(500));
            tokio::select! {
                biased;
                _ = wait_cancelled(cancel) => return Gate::Cancelled,
                _ = tokio::time::sleep(step) => {}
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Ready,
    Cancelled,
    Limited,
}

fn rate_notice(pb: &ProgressBar, wait: Duration, blocking: bool) {
    if blocking {
        pb.println(format!(
            "⏳ 服务器限流（HTTP 429），需等待约 {}；本次任务先中止（.part 已保留，稍后重跑可续传）",
            format_wait(wait)
        ));
    } else {
        pb.println(format!(
            "⏳ 服务器限流（HTTP 429），暂停 {} 后自动重试…",
            format_wait(wait)
        ));
    }
}

/// 对外暴露给下载队列：该服务器是否仍在限流窗口内、以及还要等多久。
pub fn server_rate_limited(server: &str) -> Option<Duration> {
    let gate = rate_limit_for(server);
    if gate.is_blocking() {
        Some(gate.remaining())
    } else {
        None
    }
}

/// 轮询取消标记。配合 `tokio::select!` 让挂起的网络调用也能被中断，
/// 不必等到下一个数据块到达。
async fn wait_cancelled(cancel: &AtomicBool) {
    while !cancel.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 服务器返回网页/JSON/XML 时几乎一定是登录页或错误页，不能当媒体下载。
fn html_like(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let ct = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())?
        .trim()
        .to_string();
    let lower = ct.to_ascii_lowercase();
    let bad = lower.starts_with("text/html")
        || lower.starts_with("application/json")
        || lower.starts_with("text/xml")
        || lower.starts_with("application/xml");
    bad.then_some(ct)
}

/// 目标路径锁：同一个目标文件（以及它的 `.part` / `.part.json`）同一时刻只允许
/// 一个下载任务写入。不同服务器队列可能解析到同一路径，并发写入会得到混合文件。
static DEST_LOCKS: OnceLock<std::sync::Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn dest_lock(path: &Path) -> Arc<Mutex<()>> {
    let key = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let map = DEST_LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    if map.len() > 64 {
        // 清理已经没人持有的路径锁，避免长时间运行的交互模式 无限增长。
        map.retain(|_, lock| Arc::strong_count(lock) > 1);
    }
    map.entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

async fn lock_dest(path: &Path) -> OwnedMutexGuard<()> {
    dest_lock(path).lock_owned().await
}

/// 等待目标锁期间也响应取消，避免一个卡住的任务把同路径的后续任务也拖死。
async fn lock_dest_cancellable(
    path: &Path,
    cancel: &AtomicBool,
) -> Option<OwnedMutexGuard<()>> {
    tokio::select! {
        biased;
        _ = wait_cancelled(cancel) => None,
        guard = lock_dest(path) => Some(guard),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// 精确目标文件：修复未完成任务时把下载写回原来的 `.part` 路径。
    #[serde(default)]
    pub fixed_dest: Option<PathBuf>,
}

impl Default for DownloadOpts {
    fn default() -> Self {
        Self {
            out: PathBuf::from("./downloads"),
            pattern: None,
            connections: 2,
            jobs: 2,
            limit_rate: None,
            resume: true,
            overwrite: false,
            subs: true,
            dry_run: false,
            fixed_dest: None,
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
        // 0 会让 expected=bytes/0=inf，Duration::from_secs_f64 直接 panic。
        let rate = rate.filter(|r| *r > 0);
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
        let expected = g.bytes as f64 / rate.max(1) as f64;
        if expected > elapsed {
            // 上限防止异常输入产生超大/非有限等待时间。
            Duration::from_secs_f64((expected - elapsed).min(3600.0))
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

/// 断点状态文件格式版本。旧版本（或未来无法理解的版本）一律不复用，
/// 宁可重新下载，也不能信任来历不明的完成记录。
const STATE_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize, Default)]
struct PartState {
    #[serde(default)]
    version: u32,
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
        .map_err(|e| net_error("连接直链失败", e))?;
    let status = resp.status();
    // 只有 2xx 才可能是“成功返回的网页”；401/403 等保留更具体的错误提示。
    if status.is_success()
        && let Some(ct) = html_like(resp.headers())
    {
        bail!("服务器返回的是网页/错误页（Content-Type: {ct}），不是媒体文件");
    }
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
        429 => Err(RateLimited {
            retry_after: parse_retry_after(resp.headers()),
        }
        .into()),
        other => bail!("HTTP {other}：直链不可用"),
    }
}

/// 探测直链，遇到限流时按 `Retry-After` 自动等待后重试。
///
/// 注意：限流是服务器级别的，换候选直链没有意义——调用方看到
/// `RateLimited` 后应直接结束该条目，不要继续尝试其它 URL。
async fn probe_with_retry(
    http: &reqwest::Client,
    url: &str,
    pb: &ProgressBar,
    cancel: &AtomicBool,
) -> Result<Probe> {
    let gate = rate_limit_for(url);
    let mut attempts = 0u32;
    loop {
        match gate.enter(cancel).await {
            Gate::Cancelled => return Err(anyhow::Error::new(Cancelled)),
            Gate::Limited => {
                return Err(RateLimited {
                    retry_after: Some(gate.remaining()),
                }
                .into());
            }
            Gate::Ready => {}
        }
        match probe(http, url).await {
            Ok(p) => return Ok(p),
            Err(e) => {
                let Some(rl) = e.downcast_ref::<RateLimited>() else {
                    return Err(e);
                };
                let wait = rl.retry_after.unwrap_or(DEFAULT_RATE_WAIT);
                if gate.note(wait) {
                    rate_notice(pb, wait, gate.is_blocking());
                }
                attempts += 1;
                if attempts >= MAX_RATE_RETRIES {
                    return Err(e);
                }
                // 下一轮由 gate.enter() 负责等待（短限流）或立即失败（长限流）。
            }
        }
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
        match probe_with_retry(http, url, pb, cancel.as_ref()).await {
            Ok(p) => {
                vlog(2, format!("  ↳ 直链可用: {}", emby::redact(url)));
                used_url = url.clone();
                probe_result = Some(p);
                break;
            }
            Err(e) => {
                vlog(
                    2,
                    format!(
                        "  ↳ 直链不可用: {} ({})",
                        emby::redact(url),
                        emby::redact_text(&e.to_string())
                    ),
                );
                // 限流是服务器级别的：换下一个候选 URL 只会继续消耗配额。
                if e.downcast_ref::<RateLimited>().is_some() {
                    return Err(e);
                }
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
            &state_file,
            total,
            pb,
            limiter,
            cancel.as_ref(),
            // --overwrite 的语义是“重新下载”，不能复用旧的 .part 分块。
            opts.resume && !opts.overwrite,
            probe.range_supported,
        )
        .await?
    };

    // 用户取消时绝不能把半成品 rename 成正式文件。
    if cancel.load(Ordering::SeqCst) {
        return Err(anyhow::Error::new(Cancelled));
    }

    if let Some(t) = total {
        // 分块模式已逐块校验；单连接模式在这里校验实际写入量。
        // 注意：不能只看 .part 的 length——分块模式会 set_len 预分配，
        // 长度天然等于 total，那个检查本身说明不了任何问题。
        if written != t {
            bail!(
                "文件大小不符：期望 {} 字节，实际 {} 字节（保留 .part 便于续传）",
                t,
                written
            );
        }
        let meta = tokio::fs::metadata(&part).await.context("读取临时文件失败")?;
        if meta.len() != t {
            bail!(
                "临时文件大小不符：期望 {} 字节，实际 {} 字节（保留 .part 便于续传）",
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
    // 块布局只由文件大小决定，与 `-c` 无关：调低并发不会让已有续传状态作废。
    // 如果旧状态里记录了别的块大小（老版本按 `-c` 计算），仍然优先复用它，
    // 这样升级后也能保住之前下载的进度。
    let mut chunk_size = default_chunk_size(total);
    let mut count = total.div_ceil(chunk_size);

    // 取消发生在任何块开始之前：此时 .part 还是零填充的预分配文件，
    // 绝不能继续走下去，否则长度校验会“通过”并把空文件 rename 成正式文件。
    if cancel.load(Ordering::SeqCst) {
        return Err(anyhow::Error::new(Cancelled));
    }

    // 续传：状态文件、part 长度、块布局都对得上才复用。
    //
    // 只信任 .part.json 是不安全的：例如 `-c 1` 重试会 File::create 截断
    // .part，但旧状态文件仍在；下一次分块下载会跳过“已完成”块，
    // set_len 把缺口补成零，长度校验却天然通过。
    let part_len = tokio::fs::metadata(part).await.map(|m| m.len()).ok();
    let mut completed: HashSet<u32> = HashSet::new();
    let mut resuming = false;
    if opts.resume
        && !opts.overwrite
        && part_len == Some(total)
        && let Ok(text) = tokio::fs::read_to_string(state_file).await
        && let Ok(st) = serde_json::from_str::<PartState>(&text)
        && st.version == STATE_VERSION
        && st.total == total
        && st.chunk_size > 0
        && st.chunk_size <= total
    {
        let stored_count = total.div_ceil(st.chunk_size);
        completed = st
            .completed
            .into_iter()
            .filter(|i| (*i as u64) < stored_count)
            .collect();
        chunk_size = st.chunk_size;
        count = stored_count;
        resuming = true;
        vlog(
            1,
            format!(
                "  ↳ 续传：已有 {}/{} 块（{}）",
                completed.len(),
                count,
                format_bytes(chunk_range_len(chunk_size, total, &completed))
            ),
        );
    }
    if !resuming {
        if opts.resume && !opts.overwrite && part_len.is_some() {
            vlog(
                1,
                "  ↳ .part 状态与当前文件不匹配，将重新下载（旧 .part 会被截断）",
            );
        }
        // 重新开始时清掉旧状态，避免中途中断后旧状态被误用。
        tokio::fs::remove_file(state_file).await.ok();
    }

    // 预分配；重新开始时先截断，避免残留旧字节。
    {
        let f = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(!resuming)
            .write(true)
            .open(part)
            .await
            .with_context(|| format!("打开临时文件失败: {}", part.display()))?;
        f.set_len(total).await.context("预分配文件大小失败")?;
    }

    pb.set_position(chunk_range_len(chunk_size, total, &completed));
    let done = Arc::new(Mutex::new(completed));
    let sem = Arc::new(Semaphore::new(opts.connections.max(1)));
    let mut set = tokio::task::JoinSet::new();
    let mut errors: Vec<anyhow::Error> = Vec::new();
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
        let (start, end) = chunk_range(idx, chunk_size, total);
        let permit = tokio::select! {
            biased;
            _ = wait_cancelled(cancel) => break,
            p = sem.clone().acquire_owned() => p.context("并发控制失败")?,
        };
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
            // 记状态（体积很小，先写临时文件再 rename，避免半截 JSON）
            let st = PartState {
                version: STATE_VERSION,
                total: total_copy,
                chunk_size: chunk_size_copy,
                completed: g.iter().copied().collect(),
            };
            if let Ok(json) = serde_json::to_vec(&st) {
                write_state(&state_file, &json).await;
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
                    errors.push(e);
                }
            }
            Err(e) => errors.push(anyhow!("任务崩溃: {e}")),
        }
    }

    let finished = done.lock().await.len();
    // 全部块都确认完成后才算成功：不能只依赖 .part 的长度，
    // 分块模式会 set_len 预分配，长度天然等于 total。
    if finished == count as usize && errors.is_empty() {
        pb.set_position(total);
        return Ok(total);
    }
    if cancelled || cancel.load(Ordering::SeqCst) {
        return Err(anyhow::Error::new(Cancelled));
    }
    if !errors.is_empty() {
        // 多个分块同时撞上限流时只报一条，避免刷屏；任务层再统一提示等待时间。
        if let Some(rl) = errors.iter().find_map(|e| e.downcast_ref::<RateLimited>()) {
            bail!(
                "{}（共 {} 个分块受影响；.part 与状态已保留，可直接重试续传）",
                rl,
                errors.len()
            );
        }
        let joined = errors
            .iter()
            .map(|e| format!("{e:#}"))
            .collect::<Vec<_>>()
            .join(" | ");
        bail!(
            "{} 个分块下载失败（.part 与状态已保留，可直接重试续传）：{}",
            errors.len(),
            joined
        );
    }
    bail!(
        "仅完成 {finished}/{} 个分块（.part 与状态已保留，可直接重试续传）",
        count
    );
}

/// 分块 `idx` 的字节区间（闭区间）。
fn chunk_range(idx: u64, chunk_size: u64, total: u64) -> (u64, u64) {
    let start = idx.saturating_mul(chunk_size);
    let end = (start + chunk_size).min(total).saturating_sub(1);
    (start, end)
}

fn chunk_range_len(chunk_size: u64, total: u64, completed: &HashSet<u32>) -> u64 {
    completed
        .iter()
        .map(|i| {
            let (start, end) = chunk_range(*i as u64, chunk_size, total);
            end.saturating_sub(start) + 1
        })
        .sum::<u64>()
        .min(total)
}

/// 原子写状态文件。数据块在 fetch_chunk_once 里已 fsync，
/// 状态文件本身也 fsync 后再 rename：只有数据真正落盘的分块才会被记录，
/// 避免断电后状态声称“已完成”但 .part 里是零。
async fn write_state(state_file: &Path, json: &[u8]) {
    let tmp = state_file.with_extension("json.tmp");
    let Ok(mut f) = tokio::fs::File::create(&tmp).await else {
        return;
    };
    if f.write_all(json).await.is_err() || f.sync_all().await.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
        return;
    }
    drop(f);
    // rename 失败时旧的（更保守的）状态仍然有效，最多多下一块。
    let _ = tokio::fs::rename(&tmp, state_file).await;
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
    let gate = rate_limit_for(url);
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..MAX_RETRY {
        if cancel.load(Ordering::SeqCst) {
            return Err(anyhow::Error::new(Cancelled));
        }
        match gate.enter(cancel).await {
            Gate::Cancelled => return Err(anyhow::Error::new(Cancelled)),
            Gate::Limited => {
                return Err(RateLimited {
                    retry_after: Some(gate.remaining()),
                }
                .into());
            }
            Gate::Ready => {}
        }
        match fetch_chunk_once(http, url, start, end, part, pb, limiter, cancel).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if e.downcast_ref::<Cancelled>().is_some() {
                    return Err(e);
                }
                if let Some(rl) = e.downcast_ref::<RateLimited>() {
                    let wait = rl.retry_after.unwrap_or(DEFAULT_RATE_WAIT);
                    if gate.note(wait) {
                        rate_notice(pb, wait, gate.is_blocking());
                    }
                    // 不在这里各自 sleep：下一轮 gate.enter() 统一等待，
                    // 避免几十个分块同时醒来继续冲击限流窗口。
                    last = Some(e);
                    continue;
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
    let req = http
        .get(url)
        .header(RANGE, format!("bytes={start}-{end}"));
    let resp = tokio::select! {
        biased;
        _ = wait_cancelled(cancel) => return Err(anyhow::Error::new(Cancelled)),
        r = req.send() => r.map_err(|e| net_error("建立下载连接失败", e))?,
    };
    if resp.status().as_u16() == 429 {
        return Err(RateLimited {
            retry_after: parse_retry_after(resp.headers()),
        }
        .into());
    }
    if resp.status().as_u16() != 206 {
        bail!("期望 206，实际 HTTP {}", resp.status().as_u16());
    }
    if let Some(ct) = html_like(resp.headers()) {
        bail!("服务器返回的是网页/错误页（Content-Type: {ct}），不是媒体文件");
    }
    // 校验 Content-Range 的起点，避免服务器返回了错误的区间却长度相同。
    if let Some(cr) = resp
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
    {
        let actual_start = cr
            .trim()
            .strip_prefix("bytes")
            .map(str::trim)
            .and_then(|s| s.split_once('-'))
            .and_then(|(a, _)| a.trim().parse::<u64>().ok());
        if actual_start != Some(start) {
            bail!("服务器返回的 Range 不正确：请求 {start}-{end}，实际 {cr}");
        }
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(part)
        .await
        .context("打开临时文件失败")?;
    file.seek(std::io::SeekFrom::Start(start)).await?;

    let mut written: u64 = 0;
    let mut stream = resp.bytes_stream();
    loop {
        let next = tokio::select! {
            biased;
            _ = wait_cancelled(cancel) => {
                file.flush().await.ok();
                return Err(anyhow::Error::new(Cancelled));
            }
            n = stream.next() => n,
        };
        let Some(chunk) = next else { break };
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
                return Err(net_error("传输中断", e));
            }
        }
    }
    file.flush().await.ok();
    let expected = end - start + 1;
    if written != expected {
        pb.dec(written);
        bail!("块长度不符：期望 {expected}，实际 {written}");
    }
    // 先让数据落盘，之后才允许把该块记入状态文件。
    file.sync_data().await.context("同步磁盘失败（磁盘满？）")?;
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
    state_file: &Path,
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

    let gate = rate_limit_for(url);
    let mut rate_retries = 0u32;
    let resp = loop {
        match gate.enter(cancel).await {
            Gate::Cancelled => return Err(anyhow::Error::new(Cancelled)),
            Gate::Limited => {
                return Err(RateLimited {
                    retry_after: Some(gate.remaining()),
                }
                .into());
            }
            Gate::Ready => {}
        }
        let mut req = http.get(url);
        if start > 0 {
            req = req.header(RANGE, format!("bytes={start}-"));
        }
        let resp = tokio::select! {
            biased;
            _ = wait_cancelled(cancel) => return Err(anyhow::Error::new(Cancelled)),
            r = req.send() => r.map_err(|e| net_error("建立下载连接失败", e))?,
        };
        if resp.status().as_u16() == 429 {
            let wait = parse_retry_after(resp.headers()).unwrap_or(DEFAULT_RATE_WAIT);
            if gate.note(wait) {
                rate_notice(pb, wait, gate.is_blocking());
            }
            rate_retries += 1;
            if rate_retries >= MAX_RATE_RETRIES {
                return Err(RateLimited {
                    retry_after: Some(gate.remaining()),
                }
                .into());
            }
            continue;
        }
        break resp;
    };
    let code = resp.status().as_u16();
    if start > 0 && code != 206 {
        // 服务器没按 Range 回包，只能重头来
        vlog(1, format!("  ↳ 服务器未按 Range 响应（HTTP {code}），从头下载"));
        return Box::pin(download_single(
            http, url, part, state_file, total, pb, limiter, cancel, false, false,
        ))
        .await;
    }
    if code != 200 && code != 206 {
        bail!("下载失败：HTTP {code}");
    }
    if let Some(ct) = html_like(resp.headers()) {
        bail!("服务器返回的是网页/错误页（Content-Type: {ct}），不是媒体文件");
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
        // 从头下载：清掉可能残留的分块状态，避免下次分块运行时被误用。
        tokio::fs::remove_file(state_file).await.ok();
        tokio::fs::File::create(part)
            .await
            .with_context(|| format!("创建临时文件失败: {}", part.display()))?
    };

    if let Some(t) = total.or_else(|| resp.content_length().map(|c| c + start)) {
        pb.set_length(t);
    }

    let mut written: u64 = 0;
    let mut stream = resp.bytes_stream();
    loop {
        let next = tokio::select! {
            biased;
            _ = wait_cancelled(cancel) => {
                file.flush().await.ok();
                return Err(anyhow::Error::new(Cancelled));
            }
            n = stream.next() => n,
        };
        let Some(chunk) = next else { break };
        let bytes = chunk.map_err(|e| net_error("传输中断", e))?;
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
    if let Some(t) = total
        && start + written != t
    {
        bail!(
            "文件大小不符：期望 {} 字节，实际 {} 字节（保留 .part 便于续传）",
            t,
            start + written
        );
    }
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
    let streams: Vec<&crate::model::MediaStream> = {
        // item 级别的 MediaStreams 常与第一个 MediaSource 的流重复；
        // 不去重会让同一语言的字幕被误判成多条轨道。
        let mut seen = HashSet::new();
        item.media_streams
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
            .filter(|s| {
                let key = format!(
                    "{}|{}|{}|{}",
                    s.index.unwrap_or(-1),
                    s.language.as_deref().unwrap_or(""),
                    s.codec.as_deref().unwrap_or(""),
                    s.delivery_url.as_deref().unwrap_or("")
                );
                seen.insert(key)
            })
            .collect()
    };

    if streams.is_empty() {
        return 0;
    }
    // 同一语言（含 forced）可能有多条轨道（简体/繁体/SDH），
    // 直接按语言命名会让后一条跳过或覆盖前一条。
    let mut group_counts: HashMap<(String, bool), usize> = HashMap::new();
    for s in &streams {
        let key = (
            s.language
                .clone()
                .unwrap_or_else(|| "und".into())
                .trim()
                .to_lowercase(),
            s.is_forced.unwrap_or(false),
        );
        *group_counts.entry(key).or_insert(0) += 1;
    }
    let mut seen: HashMap<(String, bool), usize> = HashMap::new();
    let http = client.http.clone();
    let mut ok = 0usize;
    for s in streams {
        let lang = s.language.clone().unwrap_or_else(|| "und".into());
        let ext = naming::subtitle_ext(s.codec.as_deref());
        let forced = s.is_forced.unwrap_or(false);
        let key = (lang.trim().to_lowercase(), forced);
        let nth = {
            let n = seen.entry(key.clone()).or_insert(0);
            *n += 1;
            *n
        };
        let disambiguator = (group_counts.get(&key).copied().unwrap_or(0) > 1)
            .then(|| nth.to_string());
        let rel = naming::subtitle_path(
            video_rel,
            &lang,
            &ext,
            forced,
            disambiguator.as_deref(),
        );
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
                Err(e) => vlog(
                    2,
                    format!(
                        "  ↳ 字幕下载失败: {}",
                        emby::redact_text(&format!("{e}"))
                    ),
                ),
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
    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| net_error("字幕连接失败", e))?;
    let status = resp.status();
    if status.as_u16() != 200 && status.as_u16() != 206 {
        return Ok(false);
    }
    // 服务器报错常返回 HTML/JSON，别把错误页存成字幕
    if html_like(resp.headers()).is_some() {
        return Ok(false);
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| net_error("读取字幕失败", e))?;
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
                eprintln!("\n! 收到中断，正在收尾（已完成分块已记录，重跑同一命令可续传）…");
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
        if let Some(wait) = server_rate_limited(&client.server) {
            eprintln!(
                "⏳ {} 服务器仍在限流，约 {} 后解除，停止派发剩余条目；稍后重跑同一命令可续传",
                client.server,
                format_wait(wait)
            );
            break;
        }
        if fail_streak.load(Ordering::SeqCst) >= 3 {
            eprintln!("! 连续 3 个条目失败，停止派发（先用 --verbose 看具体错误）");
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
                    None => client.media_urls(&item),
                };
                println!("📄 {rel}");
                println!("   → {}", dest.display());
                for u in urls {
                    println!("   ↳ {}", emby::redact(&u));
                }
                return TaskResult::Skipped;
            }

            // 同一目标路径的任务必须串行：不同服务器/重复条目可能解析到
            // 同一个文件，并发写 .part 会得到混合内容。
            let Some(_dest_guard) = lock_dest_cancellable(&dest, &cancel).await else {
                return TaskResult::Cancelled;
            };

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
                None => client.media_urls(&item),
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
                    eprintln!(
                        "✗ {rel}\n   {}",
                        emby::redact_text(&format!("{e:#}"))
                    );
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
                eprintln!("✗ 任务异常: {e}");
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
    // 修复未完成任务时直接写回原来的目标路径，才能复用已有的 .part 续传。
    // 字幕命名只用到文件名部分，因此 fixed_dest 时用目标文件名代替模板路径。
    let (dest, rel) = match &opts.fixed_dest {
        Some(dest) => {
            let name = dest
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| rel);
            (dest.clone(), name)
        }
        None => (opts.out.join(&rel), rel),
    };

    // 与 CLI 路径一致：同一目标路径串行，避免并发写同一个 .part。
    let Some(_dest_guard) = lock_dest_cancellable(&dest, cancel).await else {
        return Err(anyhow::Error::new(Cancelled));
    };

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
        None => client.media_urls(item),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_layout_is_independent_of_connections() {
        // 700 MiB 左右的文件切成约 8 块；块大小只由 total 决定。
        let total = 748_007_902u64;
        let chunk = default_chunk_size(total);
        assert_eq!(chunk, 93_500_988);
        assert_eq!(total.div_ceil(chunk), 8);

        // 超大文件的块大小封顶，避免一块失败要重下几十 GB。
        assert_eq!(default_chunk_size(100 * 1024 * 1024 * 1024), MAX_CHUNK);
        // 小文件至少一块，且不会因为 clamp 产生 0 块。
        let small = default_chunk_size(1024);
        assert_eq!(1024u64.div_ceil(small), 1);
    }

    #[test]
    fn retry_after_header_is_parsed() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(RETRY_AFTER, reqwest::header::HeaderValue::from_static("3211"));
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(3211)));

        h.insert(RETRY_AFTER, reqwest::header::HeaderValue::from_static("0"));
        assert_eq!(parse_retry_after(&h), Some(Duration::ZERO));

        // HTTP-date 形式暂不支持，应按“无 Retry-After”处理。
        h.insert(
            RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after(&h), None);

        h.remove(RETRY_AFTER);
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn format_wait_uses_readable_units() {
        assert_eq!(format_wait(Duration::from_secs(0)), "1 秒");
        assert_eq!(format_wait(Duration::from_secs(5)), "5 秒");
        assert_eq!(format_wait(Duration::from_secs(65)), "1 分 5 秒");
        assert_eq!(format_wait(Duration::from_secs(3720)), "1 小时 2 分");
    }

    #[test]
    fn short_limit_waits_and_long_limit_fails_fast() {
        let gate = RateLimitState::default();
        assert_eq!(gate.remaining(), Duration::ZERO);
        assert!(!gate.is_blocking());

        assert!(gate.note(Duration::from_secs(10)));
        assert!(!gate.is_blocking());

        // 长窗口把状态升级为“窗口内直接失败”，并延长暂停时间。
        assert!(gate.note(Duration::from_secs(400)));
        assert!(gate.is_blocking());

        // 窗口内的较短记录不会重复提示，也不会降低等待时间。
        assert!(!gate.note(Duration::from_secs(10)));
        assert!(gate.is_blocking());
    }

    #[test]
    fn chunk_ranges_cover_the_file_exactly() {
        let total = 9u64;
        let chunk = 4u64;
        let count = total.div_ceil(chunk);
        let mut covered = 0;
        for idx in 0..count {
            let (start, end) = chunk_range(idx, chunk, total);
            assert_eq!(start, idx * chunk);
            assert!(end >= start);
            assert!(end < total);
            covered += end - start + 1;
        }
        assert_eq!(covered, total);
    }

    #[test]
    fn chunk_range_len_uses_real_length_of_short_last_chunk() {
        // total=9, chunk=4 → 4 + 4 + 1
        let completed: HashSet<u32> = [0, 2].into_iter().collect();
        assert_eq!(chunk_range_len(4, 9, &completed), 5);
        let all: HashSet<u32> = [0, 1, 2].into_iter().collect();
        assert_eq!(chunk_range_len(4, 9, &all), 9);
    }
}
