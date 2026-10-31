//! 后台下载队列。
//!
//! 每个服务器一条独立的下载流：有自己的队列、并发数和限速器，互不阻塞。
//! 下载任务在后台运行，进度通过隐藏的 `ProgressBar` 暴露给交互模式。
//!
//! 队列与完成记录都会落盘（见 [`crate::store`]）：交互模式以独立进程的方式
//! 通过 Unix socket 控制本模块，因此 交互模式崩溃/关闭不会中断下载；守护进程
//! 重启时也会从 `queue.json` 恢复未完成的任务。

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use indicatif::ProgressBar;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

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
}

#[derive(Default)]
pub struct ServerState {
    pub queue: VecDeque<PendingJob>,
    pub active: Vec<ActiveJob>,
    pub max_jobs: usize,
    pub done: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub skipped: usize,
    pub subs: usize,
    pub bytes: u64,
    pub stop_requested: bool,
    /// 用户暂停：不再派发新任务，正在运行的任务已放回队首（保留 .part）。
    pub paused: bool,
    /// 重建后台任务用的连接参数（随队列一起落盘）。
    creds: Option<ClientCreds>,
}

pub struct PendingJob {
    pub id: u64,
    pub title: String,
    item: Item,
    opts: DownloadOpts,
}

pub struct ActiveJob {
    pub id: u64,
    pub title: String,
    pub pb: ProgressBar,
    pub started: Instant,
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
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ActiveView {
    pub id: u64,
    pub title: String,
    pub position: u64,
    pub length: u64,
    pub elapsed_secs: f64,
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
    PauseServer(String),
    ResumeServer(String),
    PauseAll,
    ResumeAll,
    ClearFinished,
    Completed {
        server: String,
        id: u64,
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
    limiter: Arc<Limiter>,
    cancel: Arc<AtomicBool>,
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

    pub fn clear_finished(&self) {
        let _ = self.tx.send(Cmd::ClearFinished);
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
            eprintln!("⚠️  保存下载队列失败: {e:#}");
        }
    }
}

fn snapshot_of(st: &ManagerState) -> Snapshot {
    Snapshot {
        servers: st
            .servers
            .iter()
            .map(|(name, ss)| {
                let view = ServerView {
                    queue: ss
                        .queue
                        .iter()
                        .map(|q| QueueView {
                            id: q.id,
                            title: q.title.clone(),
                        })
                        .collect(),
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

/// 把内存状态转换成可落盘格式。
fn persisted(st: &ManagerState) -> PersistedState {
    PersistedState {
        version: store::STATE_VERSION,
        seq: st.seq,
        servers: st
            .servers
            .iter()
            .map(|(name, ss)| {
                let record = |id: u64, title: &str, item: &Item, opts: &DownloadOpts| JobRecord {
                    id,
                    title: title.to_string(),
                    item: item.clone(),
                    opts: opts.clone(),
                };
                let server = PersistedServer {
                    creds: ss.creds.clone(),
                    max_jobs: ss.max_jobs,
                    queue: ss
                        .queue
                        .iter()
                        .map(|q| record(q.id, &q.title, &q.item, &q.opts))
                        .collect(),
                    active: ss
                        .active
                        .iter()
                        .map(|a| record(a.id, &a.title, &a.item, &a.opts))
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
    let config = crate::config::load_optional().ok().flatten().unwrap_or_default();

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
                    title: job.title,
                    item: job.item,
                    opts: job.opts,
                });
            }
            for job in ps.queue {
                pending.push_back(PendingJob {
                    id: job.id,
                    title: job.title,
                    item: job.item,
                    opts: job.opts,
                });
            }
            st.servers.insert(
                name,
                ServerState {
                    queue: pending,
                    active: Vec::new(),
                    max_jobs: ps.max_jobs.clamp(1, 8),
                    done: ps.done,
                    failed: ps.failed,
                    cancelled: ps.cancelled,
                    skipped: ps.skipped,
                    subs: ps.subs,
                    bytes: ps.bytes,
                    stop_requested: false,
                    paused: ps.paused,
                    creds,
                },
            );
        }

        // 历史只保留最近 50 条在内存里；完整记录仍在 history.jsonl。
        let history = store::load_history();
        let skip = history.len().saturating_sub(50);
        st.finished = history
            .into_iter()
            .skip(skip)
            .map(|h| h.entry)
            .collect();
    }

    // 后台任务需要用保存的 token 重建客户端，否则重启后无法继续下载。
    let st = state.lock().unwrap_or_else(|e| e.into_inner());
    for (name, ss) in &st.servers {
        let Some(creds) = &ss.creds else { continue };
        let Ok(client) = Client::from_creds(creds) else {
            eprintln!("⚠️  无法为 {name} 恢复下载连接，队列将等待下次入队时刷新");
            continue;
        };
        let rate = ss
            .queue
            .front()
            .map(|j| j.opts.limit_rate)
            .unwrap_or_default();
        ctx.insert(
            name.clone(),
            ServerCtx {
                client,
                limiter: Arc::new(Limiter::new(rate)),
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
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
                let mut reset_cancel = false;
                {
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    let start_id = st.seq;
                    st.seq += items.len() as u64;
                    let entry = st.servers.entry(server.clone()).or_default();
                    if entry.stop_requested {
                        entry.stop_requested = false;
                        reset_cancel = true;
                    }
                    entry.max_jobs = opts.jobs.clamp(1, 8);
                    entry.creds = Some(client.creds());
                    for (offset, item) in items.into_iter().enumerate() {
                        let title = item.title();
                        entry.queue.push_back(PendingJob {
                            id: start_id + offset as u64 + 1,
                            title,
                            item,
                            opts: opts.clone(),
                        });
                    }
                }
                // 每次都刷新 Client：重新登录后 token / 地址 / 证书设置可能已变，
                // 沿用旧 Client 会让新任务 401 或连到旧服务器。
                match ctx.get_mut(&server) {
                    Some(c) => {
                        c.client = client;
                        c.limiter = Arc::new(Limiter::new(opts.limit_rate));
                        if reset_cancel {
                            c.cancel = Arc::new(AtomicBool::new(false));
                        }
                    }
                    None => {
                        ctx.insert(
                            server.clone(),
                            ServerCtx {
                                client,
                                limiter: Arc::new(Limiter::new(opts.limit_rate)),
                                cancel: Arc::new(AtomicBool::new(false)),
                            },
                        );
                    }
                }
                dirty = true;
            }
            Cmd::CancelServer(server) => {
                if let Some(c) = ctx.get(&server) {
                    c.cancel.store(true, Ordering::SeqCst);
                }
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(ss) = st.servers.get_mut(&server) {
                    ss.stop_requested = true;
                    ss.paused = false;
                    ss.queue.clear();
                }
                drop(st);
                dirty = true;
            }
            Cmd::CancelAll => {
                for c in ctx.values() {
                    c.cancel.store(true, Ordering::SeqCst);
                }
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                for ss in st.servers.values_mut() {
                    ss.stop_requested = true;
                    ss.paused = false;
                    ss.queue.clear();
                }
                drop(st);
                dirty = true;
            }
            Cmd::PauseServer(server) => {
                if let Some(c) = ctx.get(&server) {
                    c.cancel.store(true, Ordering::SeqCst);
                }
                {
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(ss) = st.servers.get_mut(&server)
                        && !ss.paused
                    {
                        ss.paused = true;
                        requeue_active(ss);
                    }
                }
                dirty = true;
            }
            Cmd::ResumeServer(server) => {
                {
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(ss) = st.servers.get_mut(&server) {
                        ss.paused = false;
                        ss.stop_requested = false;
                    }
                }
                if let Some(c) = ctx.get_mut(&server) {
                    // 旧的取消标记已经被暂停时放回队首的任务继承；继续时换一个新的。
                    c.cancel = Arc::new(AtomicBool::new(false));
                }
                dirty = true;
            }
            Cmd::PauseAll => {
                for c in ctx.values() {
                    c.cancel.store(true, Ordering::SeqCst);
                }
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                for ss in st.servers.values_mut() {
                    if !ss.paused {
                        ss.paused = true;
                        requeue_active(ss);
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
                        ss.stop_requested = false;
                    }
                }
                for c in ctx.values_mut() {
                    c.cancel = Arc::new(AtomicBool::new(false));
                }
                dirty = true;
            }
            Cmd::ClearFinished => {
                {
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    st.finished.clear();
                }
                if let Err(e) = store::clear_history() {
                    eprintln!("⚠️  清空下载记录失败: {e:#}");
                }
            }
            Cmd::Completed {
                server,
                id,
                result,
                cancelled,
            } => {
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                let mut done = None;
                if let Some(ss) = st.servers.get_mut(&server)
                    && let Some(pos) = ss.active.iter().position(|a| a.id == id)
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
                        eprintln!("⚠️  写入下载记录失败: {e:#}");
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
        if dispatch(&state, &ctx, &tx) > 0 {
            dirty = true;
        }
        if dirty {
            let snapshot = {
                let st = state.lock().unwrap_or_else(|e| e.into_inner());
                persisted(&st)
            };
            if let Err(e) = store::save_state(&snapshot) {
                eprintln!("⚠️  保存下载队列失败: {e:#}");
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
        eprintln!("⚠️  保存下载队列失败: {e:#}");
    }
}

/// 为每个服务器启动尽可能多的下载任务（不超过该服务器自己的并发数）。
/// 返回本次派发的任务数。
fn dispatch(
    state: &Arc<Mutex<ManagerState>>,
    ctx: &BTreeMap<String, ServerCtx>,
    tx: &mpsc::UnboundedSender<Cmd>,
) -> usize {
    let mut spawns = Vec::new();
    {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        for (server, ss) in st.servers.iter_mut() {
            let Some(c) = ctx.get(server) else { continue };
            // 用户暂停后不派发新任务；继续时由 update 循环重新触发。
            if ss.paused {
                continue;
            }
            // 服务器处在长时间限流窗口内时不要继续派发，否则任务会立刻失败。
            if download::server_rate_limited(&c.client.server).is_some() {
                continue;
            }
            while ss.active.len() < ss.max_jobs.max(1) {
                let Some(job) = ss.queue.pop_front() else { break };
                let pb = ProgressBar::hidden();
                pb.set_message(job.title.clone());
                ss.active.push(ActiveJob {
                    id: job.id,
                    title: job.title.clone(),
                    pb: pb.clone(),
                    started: Instant::now(),
                    item: job.item.clone(),
                    opts: job.opts.clone(),
                });
                spawns.push((
                    server.clone(),
                    job,
                    c.client.clone(),
                    Arc::clone(&c.limiter),
                    Arc::clone(&c.cancel),
                    pb,
                ));
            }
        }
    }

    let count = spawns.len();
    for (server, job, client, limiter, cancel, pb) in spawns {
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
                id: job.id,
                result,
                cancelled,
            });
        });
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(id: u64) -> PendingJob {
        PendingJob {
            id,
            title: format!("任务 {id}"),
            item: Item {
                id: format!("item{id}"),
                ..Default::default()
            },
            opts: DownloadOpts::default(),
        }
    }

    fn active(id: u64) -> ActiveJob {
        ActiveJob {
            id,
            title: format!("任务 {id}"),
            pb: ProgressBar::hidden(),
            started: Instant::now(),
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
}
