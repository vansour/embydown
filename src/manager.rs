//! 后台下载队列。
//!
//! 每个服务器一条独立的下载流：有自己的队列、并发数和限速器，互不阻塞。
//! 下载任务在后台运行，进度通过隐藏的 `ProgressBar` 暴露给 TUI。

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use indicatif::ProgressBar;
use tokio::sync::mpsc;

use crate::download::{self, DownloadOpts, Limiter};
use crate::emby::Client;
use crate::model::Item;

#[derive(Default)]
pub struct ManagerState {
    pub servers: BTreeMap<String, ServerState>,
    /// 最近完成的下载（最多保留 50 条）
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
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FinishedStatus {
    Done,
    Skipped,
    Failed,
    Cancelled,
}

pub struct Finished {
    pub server: String,
    pub title: String,
    pub status: FinishedStatus,
    pub bytes: u64,
}

pub struct Manager {
    tx: mpsc::UnboundedSender<Cmd>,
    state: Arc<Mutex<ManagerState>>,
}

enum Cmd {
    Enqueue(Box<EnqueueCmd>),
    CancelServer(String),
    CancelAll,
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

    pub fn clear_finished(&self) {
        let _ = self.tx.send(Cmd::ClearFinished);
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, ManagerState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

async fn run(
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    tx: mpsc::UnboundedSender<Cmd>,
    state: Arc<Mutex<ManagerState>>,
) {
    let mut ctx: BTreeMap<String, ServerCtx> = BTreeMap::new();
    while let Some(cmd) = rx.recv().await {
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
            }
            Cmd::CancelServer(server) => {
                if let Some(c) = ctx.get(&server) {
                    c.cancel.store(true, Ordering::SeqCst);
                }
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(ss) = st.servers.get_mut(&server) {
                    ss.stop_requested = true;
                    ss.queue.clear();
                }
            }
            Cmd::CancelAll => {
                for c in ctx.values() {
                    c.cancel.store(true, Ordering::SeqCst);
                }
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                for ss in st.servers.values_mut() {
                    ss.stop_requested = true;
                    ss.queue.clear();
                }
            }
            Cmd::ClearFinished => {
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                st.finished.clear();
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
                    });
                }
                if let Some(f) = done {
                    st.finished.push(f);
                    if st.finished.len() > 50 {
                        st.finished.remove(0);
                    }
                }
                drop(st);
            }
        }
        dispatch(&state, &ctx, &tx);
    }
}

/// 为每个服务器启动尽可能多的下载任务（不超过该服务器自己的并发数）。
fn dispatch(
    state: &Arc<Mutex<ManagerState>>,
    ctx: &BTreeMap<String, ServerCtx>,
    tx: &mpsc::UnboundedSender<Cmd>,
) {
    let mut spawns = Vec::new();
    {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        for (server, ss) in st.servers.iter_mut() {
            let Some(c) = ctx.get(server) else { continue };
            while ss.active.len() < ss.max_jobs.max(1) {
                let Some(job) = ss.queue.pop_front() else { break };
                let pb = ProgressBar::hidden();
                pb.set_message(job.title.clone());
                ss.active.push(ActiveJob {
                    id: job.id,
                    title: job.title.clone(),
                    pb: pb.clone(),
                    started: Instant::now(),
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

    for (server, job, client, limiter, cancel, pb) in spawns {
        let tx = tx.clone();
        tokio::spawn(async move {
            use futures_util::FutureExt;
            // 下载任务内部的 panic 若逃出 tokio::spawn，就不会发送 Completed，
            // ActiveJob 会永久占用并发槽位（TUI 里显示“运行”且删不掉）。
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
}
