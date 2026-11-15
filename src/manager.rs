//! 后台下载队列。
//!
//! 每个服务器一条独立的下载流：有自己的队列、并发数和限速器，互不阻塞。
//! 下载任务在后台运行，进度通过隐藏的 `ProgressBar` 暴露给交互模式。
//!
//! 队列与完成记录都会落盘（见 [`crate::store`]）：交互模式以独立进程的方式
//! 通过 Unix socket 控制本模块，因此 交互模式崩溃/关闭不会中断下载；守护进程
//! 重启时也会从 `queue.json` 恢复未完成的任务。

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use indicatif::ProgressBar;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::download::{self, DownloadOpts, Limiter};
use crate::emby::{Client, ClientCreds};
use crate::model::Item;
use crate::store::{self, JobRecord, PersistedServer, PersistedState};

#[derive(Default)]
pub struct ManagerState {
    pub servers: BTreeMap<String, ServerState>,
    /// 最近完成的下载（最多保留 50 条；完整历史在 history.jsonl）
    pub finished: Vec<Finished>,
    seq: u64,
    /// 每次派发递增，用于区分同一任务的旧/新下载实例。
    attempt_seq: u64,
    /// 入队批次编号：同一次 enqueue 的任务共享并发/限速参数。
    batch_seq: u64,
}

#[derive(Default)]
pub struct ServerState {
    pub queue: VecDeque<PendingJob>,
    /// 失败的最近任务，可单独重试（不落盘）。
    pub retryable: VecDeque<PendingJob>,
    pub active: Vec<ActiveJob>,
    pub max_jobs: usize,
    pub done: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub skipped: usize,
    pub subs: usize,
    pub bytes: u64,
    /// 用户暂停：不再派发新任务，正在运行的任务已放回队首（保留 .part）。
    pub paused: bool,
    /// 重建后台任务用的连接参数（随队列一起落盘）。
    creds: Option<ClientCreds>,
}

pub struct PendingJob {
    pub id: u64,
    /// 同一次 `enqueue` 的子任务共享该批次编号。
    pub batch: u64,
    pub title: String,
    item: Item,
    opts: DownloadOpts,
}

pub struct ActiveJob {
    pub id: u64,
    /// 所属入队批次；限速器与批次并发上限都按它查找。
    pub batch: u64,
    /// 本次派发的唯一编号：暂停后重新派发同一任务时，旧实例的完成消息不会误伤新实例。
    pub attempt: u64,
    pub title: String,
    pub pb: ProgressBar,
    pub started: Instant,
    /// 单任务取消标记：不影响同一服务器的其他下载。
    pub cancel: Arc<AtomicBool>,
    item: Item,
    opts: DownloadOpts,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum FinishedStatus {
    Done,
    Skipped,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Finished {
    pub server: String,
    pub title: String,
    pub status: FinishedStatus,
    pub bytes: u64,
    /// 失败原因（成功/跳过时为空），交互模式里显示在标题后面。
    pub error: Option<String>,
}

// ───────────────────────── 可序列化的界面快照 ─────────────────────────

/// 守护进程每秒推给交互模式的状态。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub servers: BTreeMap<String, ServerView>,
    pub finished: Vec<Finished>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerView {
    pub queue: Vec<QueueView>,
    pub active: Vec<ActiveView>,
    /// 失败后可重试的任务。
    pub retryable: Vec<QueueView>,
    pub max_jobs: usize,
    pub done: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub skipped: usize,
    pub subs: usize,
    pub bytes: u64,
    pub paused: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QueueView {
    pub id: u64,
    pub title: String,
    /// 剧集信息：用于队列按“剧名 + 季”聚合显示。
    #[serde(default)]
    pub series: Option<String>,
    #[serde(default)]
    pub season: Option<i32>,
    #[serde(default)]
    pub episode: Option<i32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ActiveView {
    pub id: u64,
    pub title: String,
    pub position: u64,
    pub length: u64,
    pub elapsed_secs: f64,
}

/// 一次入队批次请求的文件级并发数（与 CLI 的 `--jobs` 一致）。
fn batch_limit(opts: &DownloadOpts) -> usize {
    opts.jobs.clamp(1, 8)
}

/// 该批次当前是否还能再派发一个任务。
fn batch_has_capacity(
    batch: u64,
    opts: &DownloadOpts,
    active_per_batch: &HashMap<u64, usize>,
) -> bool {
    active_per_batch.get(&batch).copied().unwrap_or(0) < batch_limit(opts)
}

/// 找到下一个可派发的任务。
///
/// 跳过已经达到自身并发上限的批次，让其它批次使用空闲槽位；
/// 同一批次内部仍保持先进先出。
fn next_dispatch_index(
    queue: &VecDeque<PendingJob>,
    active_per_batch: &HashMap<u64, usize>,
) -> Option<usize> {
    queue
        .iter()
        .position(|job| batch_has_capacity(job.batch, &job.opts, active_per_batch))
}

/// 服务器是否还有需要在下载队列里展示/持久化的状态。
///
/// 没有任务、没有历史统计、也没有暂停的空队列不应长期留在
/// `queue.json` 或界面的服务器列表里。
fn server_has_display_state(ss: &ServerState) -> bool {
    ss.paused
        || !ss.queue.is_empty()
        || !ss.active.is_empty()
        || !ss.retryable.is_empty()
        || ss.done > 0
        || ss.failed > 0
        || ss.cancelled > 0
        || ss.skipped > 0
        || ss.subs > 0
        || ss.bytes > 0
}

/// 清空下载记录与累计统计，但保留排队/运行中的任务。
fn clear_finished_state(st: &mut ManagerState) {
    st.finished.clear();
    for ss in st.servers.values_mut() {
        ss.retryable.clear();
        ss.done = 0;
        ss.failed = 0;
        ss.cancelled = 0;
        ss.skipped = 0;
        ss.subs = 0;
        ss.bytes = 0;
    }
}

/// 服务器总并发取当前队列/运行中所有批次的最大值：
/// 新批次可以调高上限，但不会把旧批次的并发悄悄降下来。
fn recompute_max_jobs(ss: &mut ServerState) {
    ss.max_jobs = ss
        .queue
        .iter()
        .map(|job| batch_limit(&job.opts))
        .chain(ss.active.iter().map(|job| batch_limit(&job.opts)))
        .max()
        .unwrap_or(1)
        .clamp(1, 8);
}

#[derive(Clone)]
pub struct Manager {
    tx: mpsc::UnboundedSender<Cmd>,
    state: Arc<Mutex<ManagerState>>,
}

enum Cmd {
    Enqueue(Box<EnqueueCmd>),
    CancelServer(String),
    CancelAll,
    CancelJob {
        server: String,
        id: u64,
    },
    RetryJob {
        server: String,
        id: u64,
    },
    PauseServer(String),
    ResumeServer(String),
    PauseAll,
    ResumeAll,
    ClearFinished {
        reply: Option<oneshot::Sender<std::result::Result<(), String>>>,
    },
    Completed {
        server: String,
        attempt: u64,
        result: Result<download::OneResult, String>,
        cancelled: bool,
    },
}

struct EnqueueCmd {
    server: String,
    client: Client,
    items: Vec<Item>,
    opts: DownloadOpts,
}

struct ServerCtx {
    client: Client,
    /// 每个入队批次一份限速器：后来的入队不会改变已排队任务的限速。
    limiters: HashMap<u64, Arc<Limiter>>,
}

impl Manager {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(ManagerState::default()));
        let shared = Arc::clone(&state);
        let reply = tx.clone();
        tokio::spawn(async move { run(rx, reply, shared).await });
        Self { tx, state }
    }

    /// 仅测试用：不启动事件循环、不读写用户配置/队列。
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self {
            tx,
            state: Arc::new(Mutex::new(ManagerState::default())),
        }
    }

    /// 把一批条目加入某个服务器的下载队列。
    pub fn enqueue(&self, server: String, client: Client, items: Vec<Item>, opts: DownloadOpts) {
        let _ = self.tx.send(Cmd::Enqueue(Box::new(EnqueueCmd {
            server,
            client,
            items,
            opts,
        })));
    }

    pub fn cancel_server(&self, server: &str) {
        let _ = self.tx.send(Cmd::CancelServer(server.to_string()));
    }

    pub fn cancel_all(&self) {
        let _ = self.tx.send(Cmd::CancelAll);
    }

    /// 取消单个排队或运行中的任务。
    pub fn cancel_job(&self, server: &str, id: u64) {
        let _ = self.tx.send(Cmd::CancelJob {
            server: server.to_string(),
            id,
        });
    }

    /// 重试一个失败的任务。
    pub fn retry_job(&self, server: &str, id: u64) {
        let _ = self.tx.send(Cmd::RetryJob {
            server: server.to_string(),
            id,
        });
    }

    pub fn pause_server(&self, server: &str) {
        let _ = self.tx.send(Cmd::PauseServer(server.to_string()));
    }

    pub fn resume_server(&self, server: &str) {
        let _ = self.tx.send(Cmd::ResumeServer(server.to_string()));
    }

    pub fn pause_all(&self) {
        let _ = self.tx.send(Cmd::PauseAll);
    }

    pub fn resume_all(&self) {
        let _ = self.tx.send(Cmd::ResumeAll);
    }

    /// 清空最近完成记录、累计统计与历史文件。
    ///
    /// 等待后台队列处理完成后再返回，确保界面/CLI 的成功提示不会早于
    /// `history.jsonl` 与 `queue.json` 的实际更新。
    pub async fn clear_finished(&self) -> std::result::Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::ClearFinished { reply: Some(reply) })
            .map_err(|_| "后台下载队列未运行".to_string())?;
        rx.await.map_err(|_| "后台下载队列未响应".to_string())?
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, ManagerState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 供交互模式轮询的可序列化快照。
    pub fn snapshot(&self) -> Snapshot {
        let st = self.state();
        snapshot_of(&st)
    }

    /// 同步落盘当前队列（守护进程退出前调用）。
    pub fn flush(&self) {
        let st = self.state();
        if let Err(e) = store::save_state(&persisted(&st)) {
            eprintln!("! 保存下载队列失败: {e:#}");
        }
    }
}

fn snapshot_of(st: &ManagerState) -> Snapshot {
    Snapshot {
        servers: st
            .servers
            .iter()
            .filter(|(_, ss)| server_has_display_state(ss))
            .map(|(name, ss)| {
                let view = ServerView {
                    queue: ss.queue.iter().map(queue_view).collect(),
                    active: ss
                        .active
                        .iter()
                        .map(|a| ActiveView {
                            id: a.id,
                            title: a.title.clone(),
                            position: a.pb.position(),
                            length: a.pb.length().unwrap_or(0),
                            elapsed_secs: a.started.elapsed().as_secs_f64(),
                        })
                        .collect(),
                    retryable: ss.retryable.iter().map(queue_view).collect(),
                    max_jobs: ss.max_jobs,
                    done: ss.done,
                    failed: ss.failed,
                    cancelled: ss.cancelled,
                    skipped: ss.skipped,
                    subs: ss.subs,
                    bytes: ss.bytes,
                    paused: ss.paused,
                };
                (name.clone(), view)
            })
            .collect(),
        finished: st.finished.clone(),
    }
}

/// 把队列任务转换成界面快照；剧集附带剧名/季/集，供队列按季聚合。
fn queue_view(job: &PendingJob) -> QueueView {
    let (series, season, episode) = if job.item.item_type.as_deref() == Some("Episode") {
        let series = job
            .item
            .series_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        match series {
            Some(series) => (
                Some(series),
                Some(job.item.season_no()),
                Some(job.item.episode_no()).filter(|episode| *episode > 0),
            ),
            None => (None, None, None),
        }
    } else {
        (None, None, None)
    };
    QueueView {
        id: job.id,
        title: job.title.clone(),
        series,
        season,
        episode,
    }
}

/// 把内存状态转换成可落盘格式。
fn persisted(st: &ManagerState) -> PersistedState {
    PersistedState {
        version: store::STATE_VERSION,
        seq: st.seq,
        servers: st
            .servers
            .iter()
            .filter(|(_, ss)| server_has_display_state(ss))
            .map(|(name, ss)| {
                let record =
                    |id: u64, batch: u64, title: &str, item: &Item, opts: &DownloadOpts| {
                        JobRecord {
                            id,
                            batch,
                            title: title.to_string(),
                            item: item.clone(),
                            opts: opts.clone(),
                        }
                    };
                let server = PersistedServer {
                    creds: ss.creds.clone(),
                    max_jobs: ss.max_jobs,
                    queue: ss
                        .queue
                        .iter()
                        .map(|q| record(q.id, q.batch, &q.title, &q.item, &q.opts))
                        .collect(),
                    active: ss
                        .active
                        .iter()
                        .map(|a| record(a.id, a.batch, &a.title, &a.item, &a.opts))
                        .collect(),
                    done: ss.done,
                    failed: ss.failed,
                    cancelled: ss.cancelled,
                    skipped: ss.skipped,
                    subs: ss.subs,
                    bytes: ss.bytes,
                    paused: ss.paused,
                };
                (name.clone(), server)
            })
            .collect(),
    }
}

/// 启动时恢复上次的队列与历史。返回是否恢复了旧队列。
fn restore(state: &Arc<Mutex<ManagerState>>, ctx: &mut BTreeMap<String, ServerCtx>) -> bool {
    let saved = store::load_state();
    let had_state = saved.is_some();
    let saved = saved.unwrap_or_default();
    let config = crate::config::load_optional()
        .ok()
        .flatten()
        .unwrap_or_default();

    {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.seq = saved.seq;
        for (name, ps) in saved.servers {
            let creds = ps.creds.clone().or_else(|| {
                config.get(&name).map(|p| ClientCreds {
                    server: p.server.clone(),
                    token: p.token.clone(),
                    user_id: p.user_id.clone(),
                    insecure: p.insecure,
                })
            });
            let mut pending = VecDeque::new();
            // 崩溃/退出时正在运行的任务优先重新排队；.part 会让它们续传。
            for job in ps.active.into_iter().rev() {
                pending.push_front(PendingJob {
                    id: job.id,
                    batch: job.batch,
                    title: job.title,
                    item: job.item,
                    opts: job.opts,
                });
            }
            for job in ps.queue {
                pending.push_back(PendingJob {
                    id: job.id,
                    batch: job.batch,
                    title: job.title,
                    item: job.item,
                    opts: job.opts,
                });
            }
            let mut restored = ServerState {
                queue: pending,
                retryable: VecDeque::new(),
                active: Vec::new(),
                max_jobs: ps.max_jobs.clamp(1, 8),
                done: ps.done,
                failed: ps.failed,
                cancelled: ps.cancelled,
                skipped: ps.skipped,
                subs: ps.subs,
                bytes: ps.bytes,
                paused: ps.paused,
                creds,
            };
            recompute_max_jobs(&mut restored);
            st.servers.insert(name, restored);
        }

        // 恢复后新入队的批次编号必须大于队列里已有的任何批次。
        st.batch_seq = st
            .servers
            .values()
            .flat_map(|ss| {
                ss.queue
                    .iter()
                    .map(|job| job.batch)
                    .chain(ss.active.iter().map(|job| job.batch))
            })
            .max()
            .unwrap_or(0);

        // 历史只保留最近 50 条在内存里；完整记录仍在 history.jsonl。
        let history = store::load_history();
        let skip = history.len().saturating_sub(50);
        st.finished = history.into_iter().skip(skip).map(|h| h.entry).collect();
    }

    // 后台任务需要用保存的 token 重建客户端，否则重启后无法继续下载。
    let st = state.lock().unwrap_or_else(|e| e.into_inner());
    for (name, ss) in &st.servers {
        let Some(creds) = &ss.creds else { continue };
        let Ok(client) = Client::from_creds(creds) else {
            eprintln!("! 无法为 {name} 恢复下载连接，队列将等待下次入队时刷新");
            continue;
        };
        let mut limiters = HashMap::new();
        for job in &ss.queue {
            limiters
                .entry(job.batch)
                .or_insert_with(|| Arc::new(Limiter::new(job.opts.limit_rate)));
        }
        for job in &ss.active {
            limiters
                .entry(job.batch)
                .or_insert_with(|| Arc::new(Limiter::new(job.opts.limit_rate)));
        }
        ctx.insert(name.clone(), ServerCtx { client, limiters });
    }
    had_state
}

/// 暂停时把运行中的任务放回队首。
///
/// 旧任务收到取消标记后会尽快退出；它们的 `Completed` 消息因为 `active`
/// 里已经没有对应 id 而被忽略，不写入历史，也不会增加失败/取消计数。
/// `.part` 与状态文件保留，继续时会从这里续传。
fn requeue_active(ss: &mut ServerState) {
    let mut jobs: Vec<PendingJob> = ss
        .active
        .drain(..)
        .map(|a| PendingJob {
            id: a.id,
            batch: a.batch,
            title: a.title,
            item: a.item,
            opts: a.opts,
        })
        .collect();
    // 倒序 push_front，保持原本的排队顺序。
    while let Some(job) = jobs.pop() {
        ss.queue.push_front(job);
    }
}

/// 取消单个任务：排队中的直接移除，运行中的设置取消标记。
fn cancel_job_in(state: &mut ManagerState, server: &str, id: u64) -> bool {
    let Some(ss) = state.servers.get_mut(server) else {
        return false;
    };
    if let Some(pos) = ss.queue.iter().position(|job| job.id == id) {
        ss.queue.remove(pos);
        return true;
    }
    if let Some(job) = ss.active.iter().find(|job| job.id == id) {
        job.cancel.store(true, Ordering::SeqCst);
        return true;
    }
    false
}

/// 把一个失败任务重新放回队尾（分配新的任务编号）。
fn retry_job_in(state: &mut ManagerState, server: &str, id: u64) -> bool {
    let new_id = state.seq + 1;
    let job = state.servers.get_mut(server).and_then(|ss| {
        let pos = ss.retryable.iter().position(|job| job.id == id)?;
        ss.retryable.remove(pos)
    });
    let Some(mut job) = job else {
        return false;
    };
    state.seq = new_id;
    job.id = new_id;
    if let Some(ss) = state.servers.get_mut(server) {
        ss.queue.push_back(job);
    }
    true
}

async fn run(
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    tx: mpsc::UnboundedSender<Cmd>,
    state: Arc<Mutex<ManagerState>>,
) {
    let mut ctx: BTreeMap<String, ServerCtx> = BTreeMap::new();
    let mut dirty = restore(&state, &mut ctx);
    // 限流窗口到期后即使没有新命令，也要重新派发队列里的任务。
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            maybe = rx.recv() => {
                let Some(cmd) = maybe else { break };
                match cmd {
            Cmd::Enqueue(cmd) => {
                let EnqueueCmd {
                    server,
                    client,
                    items,
                    opts,
                } = *cmd;
                let batch = {
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    let count = items.len() as u64;
                    let start_id = st.seq;
                    st.seq += count;
                    let batch = if count == 0 {
                        0
                    } else {
                        st.batch_seq += 1;
                        st.batch_seq
                    };
                    let entry = st.servers.entry(server.clone()).or_default();
                    entry.creds = Some(client.creds());
                    for (offset, item) in items.into_iter().enumerate() {
                        let title = item.title();
                        entry.queue.push_back(PendingJob {
                            id: start_id + offset as u64 + 1,
                            batch,
                            title,
                            item,
                            opts: opts.clone(),
                        });
                    }
                    recompute_max_jobs(entry);
                    batch
                };
                // 每次都刷新 Client：重新登录后 token / 地址 / 证书设置可能已变，
                // 沿用旧 Client 会让新任务 401 或连到旧服务器。
                // 限速器按批次保存，新批次不会改动其它批次的限速。
                match ctx.get_mut(&server) {
                    Some(c) => {
                        c.client = client;
                        if batch != 0 {
                            c.limiters
                                .insert(batch, Arc::new(Limiter::new(opts.limit_rate)));
                        }
                    }
                    None => {
                        let mut limiters = HashMap::new();
                        if batch != 0 {
                            limiters.insert(batch, Arc::new(Limiter::new(opts.limit_rate)));
                        }
                        ctx.insert(server.clone(), ServerCtx { client, limiters });
                    }
                }
                dirty = true;
            }
            Cmd::CancelServer(server) => {
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(ss) = st.servers.get_mut(&server) {
                    for job in &ss.active {
                        job.cancel.store(true, Ordering::SeqCst);
                    }
                    ss.paused = false;
                    ss.queue.clear();
                    recompute_max_jobs(ss);
                }
                drop(st);
                dirty = true;
            }
            Cmd::CancelAll => {
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                for ss in st.servers.values_mut() {
                    for job in &ss.active {
                        job.cancel.store(true, Ordering::SeqCst);
                    }
                    ss.paused = false;
                    ss.queue.clear();
                    recompute_max_jobs(ss);
                }
                drop(st);
                dirty = true;
            }
            Cmd::CancelJob { server, id } => {
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                cancel_job_in(&mut st, &server, id);
                if let Some(ss) = st.servers.get_mut(&server) {
                    recompute_max_jobs(ss);
                }
                drop(st);
                dirty = true;
            }
            Cmd::RetryJob { server, id } => {
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                retry_job_in(&mut st, &server, id);
                if let Some(ss) = st.servers.get_mut(&server) {
                    recompute_max_jobs(ss);
                }
                drop(st);
                dirty = true;
            }
            Cmd::PauseServer(server) => {
                {
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(ss) = st.servers.get_mut(&server)
                        && !ss.paused
                    {
                        ss.paused = true;
                        for job in &ss.active {
                            job.cancel.store(true, Ordering::SeqCst);
                        }
                        requeue_active(ss);
                        recompute_max_jobs(ss);
                    }
                }
                dirty = true;
            }
            Cmd::ResumeServer(server) => {
                {
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(ss) = st.servers.get_mut(&server) {
                        ss.paused = false;
                    }
                }
                dirty = true;
            }
            Cmd::PauseAll => {
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                for ss in st.servers.values_mut() {
                    if !ss.paused {
                        ss.paused = true;
                        for job in &ss.active {
                            job.cancel.store(true, Ordering::SeqCst);
                        }
                        requeue_active(ss);
                        recompute_max_jobs(ss);
                    }
                }
                drop(st);
                dirty = true;
            }
            Cmd::ResumeAll => {
                {
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    for ss in st.servers.values_mut() {
                        ss.paused = false;
                    }
                }
                dirty = true;
            }
            Cmd::ClearFinished { reply } => {
                {
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    clear_finished_state(&mut st);
                }
                let result = store::clear_history().map_err(|e| format!("{e:#}"));
                if let Err(e) = &result {
                    eprintln!("! 清空下载记录失败: {e}");
                }
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
                // 累计计数也归零，必须落盘，否则重启后又会显示旧统计。
                dirty = true;
            }
            Cmd::Completed {
                server,
                attempt,
                result,
                cancelled,
            } => {
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                let mut done = None;
                if let Some(ss) = st.servers.get_mut(&server)
                    && let Some(pos) = ss.active.iter().position(|a| a.attempt == attempt)
                {
                    let job = ss.active.remove(pos);
                    let status = match &result {
                        Ok(r) if r.skipped => {
                            ss.skipped += 1;
                            FinishedStatus::Skipped
                        }
                        Ok(r) => {
                            ss.done += 1;
                            ss.subs += r.subs;
                            ss.bytes += r.bytes;
                            FinishedStatus::Done
                        }
                        Err(_) if cancelled => {
                            ss.cancelled += 1;
                            FinishedStatus::Cancelled
                        }
                        Err(_) => {
                            ss.failed += 1;
                            FinishedStatus::Failed
                        }
                    };
                    if status == FinishedStatus::Failed {
                        let ActiveJob {
                            id,
                            batch,
                            title,
                            item,
                            opts,
                            ..
                        } = &job;
                        ss.retryable.push_back(PendingJob {
                            id: *id,
                            batch: *batch,
                            title: title.clone(),
                            item: item.clone(),
                            opts: opts.clone(),
                        });
                        if ss.retryable.len() > 50 {
                            ss.retryable.pop_front();
                        }
                    }
                    recompute_max_jobs(ss);
                    done = Some(Finished {
                        server: server.clone(),
                        title: job.title,
                        status,
                        bytes: result.as_ref().map(|r| r.bytes).unwrap_or(0),
                        error: result.as_ref().err().cloned(),
                    });
                }
                if let Some(f) = done {
                    // 先落盘再返回：即使紧接着进程崩溃，记录也不会丢。
                    if let Err(e) = store::append_history(&f) {
                        eprintln!("! 写入下载记录失败: {e:#}");
                    }
                    st.finished.push(f);
                    if st.finished.len() > 50 {
                        st.finished.remove(0);
                    }
                }
                drop(st);
                dirty = true;
            }
                }
            }
            _ = tick.tick() => {}
        }
        if dispatch(&state, &mut ctx, &tx) > 0 {
            dirty = true;
        }
        prune_limiters(&state, &mut ctx);
        if dirty {
            let snapshot = {
                let st = state.lock().unwrap_or_else(|e| e.into_inner());
                persisted(&st)
            };
            if let Err(e) = store::save_state(&snapshot) {
                eprintln!("! 保存下载队列失败: {e:#}");
            }
            dirty = false;
        }
    }

    // 所有发送端都退出（守护进程收尾）：把最终队列落盘。
    let snapshot = {
        let st = state.lock().unwrap_or_else(|e| e.into_inner());
        persisted(&st)
    };
    if let Err(e) = store::save_state(&snapshot) {
        eprintln!("! 保存下载队列失败: {e:#}");
    }
}

/// 为每个服务器启动尽可能多的下载任务（不超过该服务器自己的并发数）。
///
/// 每个批次还有自己的文件级并发上限；队首批次达到上限时暂停派发，
/// 以保持 FIFO 顺序，避免后面的批次插队。
/// 返回本次派发的任务数。
fn dispatch(
    state: &Arc<Mutex<ManagerState>>,
    ctx: &mut BTreeMap<String, ServerCtx>,
    tx: &mpsc::UnboundedSender<Cmd>,
) -> usize {
    let mut spawns = Vec::new();
    {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        let mut attempt_seq = st.attempt_seq;
        for (server, ss) in st.servers.iter_mut() {
            let Some(c) = ctx.get_mut(server) else {
                continue;
            };
            recompute_max_jobs(ss);
            // 用户暂停后不派发新任务；继续时由 update 循环重新触发。
            if ss.paused {
                continue;
            }
            // 服务器处在长时间限流窗口内时不要继续派发，否则任务会立刻失败。
            if download::server_rate_limited(&c.client.server).is_some() {
                continue;
            }
            let mut active_per_batch: HashMap<u64, usize> = HashMap::new();
            for job in &ss.active {
                *active_per_batch.entry(job.batch).or_insert(0) += 1;
            }
            while ss.active.len() < ss.max_jobs.max(1) {
                let Some(index) = next_dispatch_index(&ss.queue, &active_per_batch) else {
                    break;
                };
                let job = ss.queue.remove(index).expect("索引来自队列");
                let limiter = c
                    .limiters
                    .entry(job.batch)
                    .or_insert_with(|| Arc::new(Limiter::new(job.opts.limit_rate)))
                    .clone();
                let pb = ProgressBar::hidden();
                pb.set_message(job.title.clone());
                let cancel = Arc::new(AtomicBool::new(false));
                attempt_seq += 1;
                let attempt = attempt_seq;
                *active_per_batch.entry(job.batch).or_insert(0) += 1;
                ss.active.push(ActiveJob {
                    id: job.id,
                    batch: job.batch,
                    attempt,
                    title: job.title.clone(),
                    pb: pb.clone(),
                    started: Instant::now(),
                    cancel: Arc::clone(&cancel),
                    item: job.item.clone(),
                    opts: job.opts.clone(),
                });
                spawns.push((
                    server.clone(),
                    job,
                    attempt,
                    c.client.clone(),
                    limiter,
                    cancel,
                    pb,
                ));
            }
        }
        st.attempt_seq = attempt_seq;
    }

    let count = spawns.len();
    for (server, job, attempt, client, limiter, cancel, pb) in spawns {
        let tx = tx.clone();
        tokio::spawn(async move {
            use futures_util::FutureExt;
            // 下载任务内部的 panic 若逃出 tokio::spawn，就不会发送 Completed，
            // ActiveJob 会永久占用并发槽位（交互模式里显示“运行”且删不掉）。
            let outcome = std::panic::AssertUnwindSafe(download::download_one(
                &client, &job.item, &job.opts, &limiter, &cancel, &pb,
            ))
            .catch_unwind()
            .await;
            let (result, cancelled) = match outcome {
                Ok(res) => {
                    let cancelled = res
                        .as_ref()
                        .err()
                        .map(download::is_cancelled)
                        .unwrap_or(false);
                    (
                        res.map_err(|e| crate::emby::redact_text(&format!("{e:#}"))),
                        cancelled,
                    )
                }
                Err(_) => (Err("任务崩溃（内部错误）".to_string()), false),
            };
            let _ = tx.send(Cmd::Completed {
                server,
                attempt,
                result,
                cancelled,
            });
        });
    }
    count
}

/// 清理已经没有排队/运行任务的批次限速器，避免长时间运行时无限增长。
fn prune_limiters(state: &Arc<Mutex<ManagerState>>, ctx: &mut BTreeMap<String, ServerCtx>) {
    let live: BTreeMap<String, HashSet<u64>> = {
        let st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.servers
            .iter()
            .map(|(name, ss)| {
                let batches = ss
                    .queue
                    .iter()
                    .map(|job| job.batch)
                    .chain(ss.active.iter().map(|job| job.batch))
                    .collect();
                (name.clone(), batches)
            })
            .collect()
    };
    for (name, c) in ctx.iter_mut() {
        match live.get(name) {
            Some(batches) => c.limiters.retain(|batch, _| batches.contains(batch)),
            None => c.limiters.clear(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(id: u64) -> PendingJob {
        pending_batch(id, 1, 2)
    }

    fn pending_batch(id: u64, batch: u64, jobs: usize) -> PendingJob {
        PendingJob {
            id,
            batch,
            title: format!("任务 {id}"),
            item: Item {
                id: format!("item{id}"),
                ..Default::default()
            },
            opts: DownloadOpts {
                jobs,
                ..Default::default()
            },
        }
    }

    fn active(id: u64) -> ActiveJob {
        ActiveJob {
            id,
            batch: 1,
            attempt: id,
            title: format!("任务 {id}"),
            pb: ProgressBar::hidden(),
            started: Instant::now(),
            cancel: Arc::new(AtomicBool::new(false)),
            item: Item {
                id: format!("item{id}"),
                ..Default::default()
            },
            opts: DownloadOpts::default(),
        }
    }

    #[test]
    fn pausing_requeues_active_jobs_in_order() {
        let mut ss = ServerState::default();
        ss.queue.push_back(pending(3));
        ss.active.push(active(1));
        ss.active.push(active(2));
        ss.paused = true;

        requeue_active(&mut ss);

        // 运行中的任务放回队首，并保持原本顺序；暂停状态不变。
        assert!(ss.active.is_empty());
        let ids: Vec<u64> = ss.queue.iter().map(|j| j.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert!(ss.paused);
    }

    #[test]
    fn later_batches_do_not_lower_server_concurrency() {
        let mut ss = ServerState::default();
        ss.queue.push_back(pending_batch(1, 1, 4));
        ss.queue.push_back(pending_batch(2, 2, 2));
        recompute_max_jobs(&mut ss);
        assert_eq!(ss.max_jobs, 4);

        // 队列清空后回落到默认值 1。
        ss.queue.clear();
        recompute_max_jobs(&mut ss);
        assert_eq!(ss.max_jobs, 1);
    }

    #[test]
    fn each_batch_has_its_own_concurrency_limit() {
        let job = pending_batch(1, 7, 3);
        let mut active = HashMap::new();
        assert!(batch_has_capacity(job.batch, &job.opts, &active));
        active.insert(7, 2);
        assert!(batch_has_capacity(job.batch, &job.opts, &active));
        active.insert(7, 3);
        assert!(!batch_has_capacity(job.batch, &job.opts, &active));
        // 其它批次不受影响。
        assert!(batch_has_capacity(8, &job.opts, &active));
    }

    #[test]
    fn dispatch_skips_batches_that_are_already_full() {
        let mut queue = VecDeque::new();
        queue.push_back(pending_batch(1, 1, 1));
        queue.push_back(pending_batch(2, 2, 1));
        let mut active = HashMap::new();

        assert_eq!(next_dispatch_index(&queue, &active), Some(0));
        active.insert(1, 1);
        // 批次 1 已满，跳到批次 2 的任务。
        assert_eq!(next_dispatch_index(&queue, &active), Some(1));
        active.insert(2, 1);
        assert_eq!(next_dispatch_index(&queue, &active), None);
    }

    #[test]
    fn cancel_and_retry_single_jobs() {
        let mut st = ManagerState::default();
        let mut ss = ServerState::default();
        ss.queue.push_back(pending(1));
        ss.queue.push_back(pending(2));
        ss.active.push(active(3));
        ss.retryable.push_back(pending(4));
        st.servers.insert("media".into(), ss);
        st.seq = 10;

        // 排队任务直接移除；运行中的任务只设置自己的取消标记。
        assert!(cancel_job_in(&mut st, "media", 1));
        let ids: Vec<u64> = st.servers["media"].queue.iter().map(|j| j.id).collect();
        assert_eq!(ids, vec![2]);
        assert!(cancel_job_in(&mut st, "media", 3));
        assert!(st.servers["media"].active[0].cancel.load(Ordering::SeqCst));
        assert!(!cancel_job_in(&mut st, "media", 99));

        // 重试分配新编号并移到队尾。
        assert!(retry_job_in(&mut st, "media", 4));
        assert_eq!(st.servers["media"].queue.back().unwrap().id, 11);
        assert!(st.servers["media"].retryable.is_empty());
        assert_eq!(st.seq, 11);
        assert!(!retry_job_in(&mut st, "media", 4));
    }

    #[test]
    fn clearing_records_resets_counters_but_keeps_queue() {
        let mut st = ManagerState::default();
        let mut ss = ServerState::default();
        ss.queue.push_back(pending(1));
        ss.active.push(active(2));
        ss.retryable.push_back(pending(3));
        ss.done = 5;
        ss.failed = 2;
        ss.cancelled = 1;
        ss.skipped = 3;
        ss.subs = 4;
        ss.bytes = 99;
        st.servers.insert("media".into(), ss);
        st.finished.push(Finished {
            server: "media".into(),
            title: "旧记录".into(),
            status: FinishedStatus::Done,
            bytes: 10,
            error: None,
        });

        clear_finished_state(&mut st);

        let ss = &st.servers["media"];
        assert!(st.finished.is_empty());
        assert!(ss.retryable.is_empty());
        assert_eq!((ss.done, ss.failed, ss.cancelled), (0, 0, 0));
        assert_eq!((ss.skipped, ss.subs, ss.bytes), (0, 0, 0));
        // 排队/运行中的任务不能被清掉。
        assert_eq!(ss.queue.len(), 1);
        assert_eq!(ss.active.len(), 1);
    }

    #[test]
    fn empty_servers_are_omitted_from_snapshot_and_queue_file() {
        let mut st = ManagerState::default();
        st.servers.insert("empty".into(), ServerState::default());
        let mut active = ServerState::default();
        active.queue.push_back(pending(1));
        st.servers.insert("active".into(), active);

        let snapshot = snapshot_of(&st);
        assert!(!snapshot.servers.contains_key("empty"));
        assert!(snapshot.servers.contains_key("active"));

        let persisted = persisted(&st);
        assert!(!persisted.servers.contains_key("empty"));
        assert!(persisted.servers.contains_key("active"));
    }
}
