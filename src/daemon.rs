//! 后台下载守护进程。
//!
//! 交互模式不再自己执行下载：它只通过 `$XDG_RUNTIME_DIR/embydown/daemon.sock`
//! 发送「入队 / 取消 / 查询」请求。守护进程是独立的 setsid 子进程，所以：
//! - 交互模式崩溃、Ctrl+C、直接关掉终端，下载都不受影响；
//! - 队列与完成记录由守护进程落盘，重启后可从断点继续。
//!
//! 单实例：socket + `daemon.lock`（PID 锁）双重保证不会重复下载同一批文件。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::SignalKind;
use tokio::sync::{mpsc, oneshot};

use crate::download::DownloadOpts;
use crate::emby::{Client, ClientCreds};
use crate::manager::{Manager, Snapshot};
use crate::model::Item;
use crate::store;

// ───────────────────────── 协议 ─────────────────────────

/// 后台服务协议版本。
///
/// 0.0.3 及更早的守护进程没有这个字段；交互模式检测到旧服务后会先让它优雅退出，
/// 再拉起当前版本的守护进程（队列已落盘，会继续下载）。
/// 新增/修改请求或快照字段时递增。
const PROTOCOL_VERSION: u32 = 4;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Enqueue {
        server: String,
        client: ClientCreds,
        items: Vec<Item>,
        opts: Box<DownloadOpts>,
    },
    CancelServer {
        server: String,
    },
    CancelAll,
    CancelJob {
        server: String,
        id: u64,
    },
    RetryJob {
        server: String,
        id: u64,
    },
    PauseServer {
        server: String,
    },
    ResumeServer {
        server: String,
    },
    PauseAll,
    ResumeAll,
    ClearHistory,
    Snapshot,
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<Snapshot>,
    /// 后台服务协议版本；旧版守护进程没有这个字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    protocol: Option<u32>,
}

impl Response {
    fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            state: None,
            protocol: Some(PROTOCOL_VERSION),
        }
    }

    fn state(state: Snapshot) -> Self {
        Self {
            ok: true,
            error: None,
            state: Some(state),
            protocol: Some(PROTOCOL_VERSION),
        }
    }

    fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            state: None,
            protocol: Some(PROTOCOL_VERSION),
        }
    }
}

// ───────────────────────── 守护进程 ─────────────────────────

/// 运行后台下载服务（`embydown daemon`），通常由交互模式自动拉起。
pub async fn run() -> Result<()> {
    let socket = store::socket_path()?;
    let lock_path = store::lock_path()?;
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建运行目录失败: {}", dir.display()))?;
    }

    let Some(_lock) = acquire_instance_lock(&lock_path)? else {
        println!("后台下载服务已在运行");
        return Ok(());
    };

    // 拿到单实例锁后，残留的 socket 一定是上次异常退出留下的。
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("监听 {} 失败", socket.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).ok();

    // SIGHUP：终端关闭时不要退出；SIGINT/SIGTERM：优雅收尾并把队列落盘。
    let mut sighup = signal(SignalKind::hangup())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    let manager = Arc::new(Manager::new());
    loop {
        tokio::select! {
            _ = sighup.recv() => {
                // 终端关闭产生的挂断信号：忽略，继续下载。
            }
            _ = sigint.recv() => break,
            _ = sigterm.recv() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let manager = Arc::clone(&manager);
                        tokio::spawn(async move {
                            if let Err(e) = serve(stream, manager).await {
                                eprintln!("后台连接异常: {e:#}");
                            }
                        });
                    }
                    Err(e) => eprintln!("接受后台连接失败: {e}"),
                }
            }
        }
    }

    manager.flush();
    drop(listener);
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

async fn serve(stream: UnixStream, manager: Arc<Manager>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle(req, &manager),
            Err(e) => Response::err(format!("无法解析请求: {e}")),
        };
        let mut out = serde_json::to_vec(&response).context("序列化响应失败")?;
        out.push(b'\n');
        writer.write_all(&out).await?;
        writer.flush().await?;
    }
    Ok(())
}

fn handle(req: Request, manager: &Manager) -> Response {
    match req {
        Request::Enqueue {
            server,
            client,
            items,
            opts,
        } => match Client::from_creds(&client) {
            Ok(client) => {
                manager.enqueue(server, client, items, *opts);
                Response::ok()
            }
            Err(e) => Response::err(format!("{e:#}")),
        },
        Request::CancelServer { server } => {
            manager.cancel_server(&server);
            Response::ok()
        }
        Request::CancelAll => {
            manager.cancel_all();
            Response::ok()
        }
        Request::CancelJob { server, id } => {
            let state = manager.snapshot();
            let exists = state
                .servers
                .get(&server)
                .map(|s| {
                    s.queue.iter().any(|job| job.id == id)
                        || s.active.iter().any(|job| job.id == id)
                })
                .unwrap_or(false);
            if exists {
                manager.cancel_job(&server, id);
                Response::ok()
            } else {
                Response::err(format!("任务 {id} 不存在或已结束"))
            }
        }
        Request::RetryJob { server, id } => {
            let state = manager.snapshot();
            let exists = state
                .servers
                .get(&server)
                .map(|s| s.retryable.iter().any(|job| job.id == id))
                .unwrap_or(false);
            if exists {
                manager.retry_job(&server, id);
                Response::ok()
            } else {
                Response::err(format!("任务 {id} 不可重试"))
            }
        }
        Request::PauseServer { server } => {
            if manager.snapshot().servers.contains_key(&server) {
                manager.pause_server(&server);
                Response::ok()
            } else {
                Response::err(format!("没有名为 {server} 的下载队列"))
            }
        }
        Request::ResumeServer { server } => {
            if manager.snapshot().servers.contains_key(&server) {
                manager.resume_server(&server);
                Response::ok()
            } else {
                Response::err(format!("没有名为 {server} 的下载队列"))
            }
        }
        Request::PauseAll => {
            manager.pause_all();
            Response::ok()
        }
        Request::ResumeAll => {
            manager.resume_all();
            Response::ok()
        }
        Request::ClearHistory => {
            manager.clear_finished();
            Response::ok()
        }
        Request::Snapshot => Response::state(manager.snapshot()),
        Request::Ping => Response::ok(),
    }
}

fn signal(kind: SignalKind) -> Result<tokio::signal::unix::Signal> {
    tokio::signal::unix::signal(kind).context("注册信号处理失败")
}

/// PID 锁：`create_new` 是原子的，保证同一时刻只有一个守护进程。
struct InstanceLock {
    path: PathBuf,
    _file: std::fs::File,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn acquire_instance_lock(path: &Path) -> Result<Option<InstanceLock>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建运行目录失败: {}", dir.display()))?;
    }
    for _ in 0..50 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(mut file) => {
                let _ = write!(file, "{}", std::process::id());
                return Ok(Some(InstanceLock {
                    path: path.to_path_buf(),
                    _file: file,
                }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_is_stale(path) {
                    let _ = std::fs::remove_file(path);
                    continue;
                }
                return Ok(None);
            }
            Err(e) => {
                return Err(e).with_context(|| format!("创建锁文件失败: {}", path.display()))
            }
        }
    }
    Ok(None)
}

fn lock_is_stale(path: &Path) -> bool {
    if !Path::new("/proc").is_dir() {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return false;
    };
    !process_is_running(pid)
}

/// 进程是否仍在运行。
///
/// 只检查 `/proc/{pid}` 是否存在不够：进程被 SIGTERM 结束后，父进程还没来得及
/// 回收时会短暂变成僵尸（Z），此时 `/proc/{pid}` 仍然存在，会让新守护进程误判
/// “已有实例在运行”而直接退出。
fn process_is_running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // 格式：`pid (comm) state ...`，comm 里可能有空格或括号，取最后一个 ')'。
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    !rest.trim_start().starts_with('Z')
}

// ───────────────────────── 界面侧客户端 ─────────────────────────

/// 交互模式发往后端的命令；需要确认的操作通过 `reply` 等待回执。
struct UiCommand {
    request: Request,
    reply: Option<oneshot::Sender<Result<()>>>,
}

/// 交互模式持有的后台服务句柄：缓存最新快照，后台任务负责连接/重连。
pub struct DaemonHandle {
    snapshot: Arc<Mutex<Snapshot>>,
    error: Arc<Mutex<Option<String>>>,
    tx: mpsc::UnboundedSender<UiCommand>,
}

impl DaemonHandle {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let error = Arc::new(Mutex::new(None));
        let snap = Arc::clone(&snapshot);
        let err = Arc::clone(&error);
        tokio::spawn(async move { connection_task(rx, snap, err).await });
        Self {
            snapshot,
            error,
            tx,
        }
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, Snapshot> {
        self.snapshot.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 取出并清空最近一次后台通信错误（由交互模式状态栏显示）。
    pub fn take_error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn send(&self, req: Request) {
        let _ = self.tx.send(UiCommand {
            request: req,
            reply: None,
        });
    }

    /// 发送命令并等待后台确认（成功/失败）。
    async fn call(&self, req: Request) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(UiCommand {
                request: req,
                reply: Some(reply),
            })
            .map_err(|_| anyhow::anyhow!("后台下载服务未连接"))?;
        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => bail!("后台下载服务未响应"),
            Err(_) => bail!("后台下载服务响应超时"),
        }
    }

    pub fn enqueue(&self, server: String, client: Client, items: Vec<Item>, opts: DownloadOpts) {
        self.send(Request::Enqueue {
            server,
            client: client.creds(),
            items,
            opts: Box::new(opts),
        });
    }

    pub async fn cancel_server(&self, server: &str) -> Result<()> {
        self.call(Request::CancelServer {
            server: server.to_string(),
        })
        .await
    }

    pub async fn cancel_all(&self) -> Result<()> {
        self.call(Request::CancelAll).await
    }

    pub async fn cancel_job(&self, server: &str, id: u64) -> Result<()> {
        self.call(Request::CancelJob {
            server: server.to_string(),
            id,
        })
        .await
    }

    pub async fn retry_job(&self, server: &str, id: u64) -> Result<()> {
        self.call(Request::RetryJob {
            server: server.to_string(),
            id,
        })
        .await
    }

    pub async fn pause_server(&self, server: &str) -> Result<()> {
        self.call(Request::PauseServer {
            server: server.to_string(),
        })
        .await
    }

    pub async fn resume_server(&self, server: &str) -> Result<()> {
        self.call(Request::ResumeServer {
            server: server.to_string(),
        })
        .await
    }

    pub async fn pause_all(&self) -> Result<()> {
        self.call(Request::PauseAll).await
    }

    pub async fn resume_all(&self) -> Result<()> {
        self.call(Request::ResumeAll).await
    }

    pub async fn clear_finished(&self) -> Result<()> {
        self.call(Request::ClearHistory).await
    }
}

/// CLI：暂停后台下载队列；`server` 为 `None` 时暂停全部。
pub async fn cli_pause(server: Option<&str>) -> Result<()> {
    send_cli(match server {
        Some(server) => Request::PauseServer {
            server: server.to_string(),
        },
        None => Request::PauseAll,
    })
    .await
}

/// CLI：继续后台下载队列；`server` 为 `None` 时继续全部。
pub async fn cli_resume(server: Option<&str>) -> Result<()> {
    send_cli(match server {
        Some(server) => Request::ResumeServer {
            server: server.to_string(),
        },
        None => Request::ResumeAll,
    })
    .await
}

async fn send_cli(request: Request) -> Result<()> {
    let mut conn = connect_daemon().await?;
    let resp = conn.request(&request).await?;
    if !resp.ok {
        bail!(resp.error.unwrap_or_else(|| "后台服务返回未知错误".into()));
    }
    Ok(())
}

async fn connection_task(
    mut rx: mpsc::UnboundedReceiver<UiCommand>,
    snapshot: Arc<Mutex<Snapshot>>,
    error: Arc<Mutex<Option<String>>>,
) {
    loop {
        if rx.is_closed() {
            break;
        }
        match run_connection(&mut rx, &snapshot).await {
            Ok(()) => {}
            Err(e) => {
                set_error(&error, format!("后台下载服务连接失败：{e:#}"));
                // 连接断开时确保守护进程仍在运行（可能被系统清理/崩溃）。
                if let Err(e) = ensure_daemon().await {
                    set_error(&error, format!("后台下载服务启动失败：{e:#}"));
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

fn set_error(slot: &Arc<Mutex<Option<String>>>, text: String) {
    eprintln!("{text}");
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(text);
}

/// 与后台服务的一条连接（已完成协议握手）。
struct Connection {
    writer: OwnedWriteHalf,
    lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
}

impl Connection {
    async fn request(&mut self, request: &Request) -> Result<Response> {
        let mut out = serde_json::to_vec(request).context("序列化请求失败")?;
        out.push(b'\n');
        self.writer
            .write_all(&out)
            .await
            .context("发送请求到后台服务失败")?;
        self.writer.flush().await.ok();

        let line = tokio::time::timeout(Duration::from_secs(5), self.lines.next_line())
            .await
            .context("后台下载服务响应超时")??
            .context("后台下载服务断开连接")?;
        serde_json::from_str(&line).context("解析后台响应失败")
    }
}

/// 连接后台服务并完成协议握手。
///
/// 0.0.3 及更早的守护进程没有 `protocol` 字段；检测到旧服务时先让它优雅退出
/// （队列会落盘），再拉起当前版本并重连一次。
async fn connect_daemon() -> Result<Connection> {
    ensure_daemon().await?;
    let socket = store::socket_path()?;
    for attempt in 0..2 {
        let stream = UnixStream::connect(&socket)
            .await
            .with_context(|| format!("连接 {} 失败", socket.display()))?;
        let (reader, writer) = stream.into_split();
        let mut conn = Connection {
            writer,
            lines: BufReader::new(reader).lines(),
        };
        let hello = conn.request(&Request::Ping).await?;
        if hello.protocol == Some(PROTOCOL_VERSION) {
            return Ok(conn);
        }
        drop(conn);
        if attempt == 1 {
            bail!("后台下载服务版本不匹配，请手动结束旧服务后重试");
        }
        stop_old_daemon().await?;
        ensure_daemon().await?;
    }
    unreachable!("最多尝试两次连接")
}

async fn run_connection(
    rx: &mut mpsc::UnboundedReceiver<UiCommand>,
    snapshot: &Arc<Mutex<Snapshot>>,
) -> Result<()> {
    let mut conn = connect_daemon().await?;

    // 定期拉取快照；界面发来的命令优先处理。
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let command = tokio::select! {
            _ = tick.tick() => None,
            // 界面/交互模式正常退出时句柄被释放，连接任务也安静结束。
            cmd = rx.recv() => match cmd {
                Some(cmd) => Some(cmd),
                None => return Ok(()),
            },
        };
        let (request, reply) = match command {
            Some(UiCommand { request, reply }) => (request, reply),
            None => (Request::Snapshot, None),
        };
        let resp = conn.request(&request).await?;
        if let Some(state) = resp.state {
            *snapshot.lock().unwrap_or_else(|e| e.into_inner()) = state;
        }
        match reply {
            Some(reply) => {
                let result = if resp.ok {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        resp.error
                            .unwrap_or_else(|| "后台服务返回未知错误".to_string())
                    ))
                };
                let _ = reply.send(result);
            }
            None => {
                if !resp.ok {
                    bail!(resp.error.unwrap_or_else(|| "后台服务返回未知错误".into()));
                }
            }
        }
    }
}

/// 连接不上就拉起守护进程，并等待 socket 可用。
async fn ensure_daemon() -> Result<()> {
    let socket = store::socket_path()?;
    if UnixStream::connect(&socket).await.is_ok() {
        return Ok(());
    }
    spawn_daemon()?;
    for _ in 0..100 {
        if UnixStream::connect(&socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!(
        "后台服务启动超时（可运行 `embydown daemon` 查看错误，日志见 {}）",
        store::log_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "daemon.log".into())
    )
}

/// 旧版本守护进程不认识暂停/继续命令：让它优雅退出，由调用方重新拉起。
///
/// SIGTERM 会让守护进程先保存队列，再删除 socket 与锁文件；正在运行的任务
/// 在下次启动时回到队首并续传。
async fn stop_old_daemon() -> Result<()> {
    let lock = store::lock_path()?;
    let text = std::fs::read_to_string(&lock)
        .with_context(|| format!("读取后台服务锁 {} 失败", lock.display()))?;
    let pid: u32 = text.trim().parse().context("后台服务锁内容不是有效 PID")?;

    // 防止 PID 复用：只结束命令行确实是 embydown daemon 的进程。
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let cmdline = String::from_utf8_lossy(&cmdline);
    if !cmdline.contains("embydown") || !cmdline.contains("daemon") {
        bail!("检测到旧版后台服务（PID {pid}），但无法确认进程身份；请手动结束后重试");
    }

    let status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .context("发送 SIGTERM 失败")?;
    if !status.success() {
        bail!("结束旧版后台服务（PID {pid}）失败");
    }
    for _ in 0..60 {
        if !process_is_running(pid) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!("旧版后台服务（PID {pid}）未在 3 秒内退出")
}

/// 自更新完成后让后台服务改用新二进制；没有在运行时返回 `false`。
///
/// 正在下载的任务会保留 `.part` 并在下次启动时续传。
pub async fn stop_running_daemon() -> Result<bool> {
    let lock = store::lock_path()?;
    if !lock.exists() {
        return Ok(false);
    }
    let Ok(text) = std::fs::read_to_string(&lock) else {
        return Ok(false);
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return Ok(false);
    };
    if !process_is_running(pid) {
        let _ = std::fs::remove_file(&lock);
        return Ok(false);
    }
    stop_old_daemon().await?;
    Ok(true)
}

/// 以独立进程组启动 `embydown daemon`：不随交互模式一起被 Ctrl+C 结束。
fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe().context("无法定位 embydown 可执行文件")?;
    let log = store::log_path()?;
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建日志目录失败: {}", dir.display()))?;
    }
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log)
        .with_context(|| format!("打开后台日志 {} 失败", log.display()))?;
    let stderr = stdout.try_clone().context("复制后台日志句柄失败")?;
    Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        // 新进程组：终端里的 Ctrl+C 不会发给它（守护进程自己还忽略 SIGHUP）。
        .process_group(0)
        .spawn()
        .context("启动后台下载服务失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_as_json_lines() {
        let req = Request::Enqueue {
            server: "srv".into(),
            client: ClientCreds {
                server: "http://host:8096".into(),
                token: "t".into(),
                user_id: "u".into(),
                insecure: false,
            },
            items: vec![Item {
                id: "1".into(),
                name: Some("电影".into()),
                direct_url: Some("http://host/x?api_key=k".into()),
                ..Default::default()
            }],
            opts: Box::new(DownloadOpts::default()),
        };
        let text = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&text).unwrap();
        match back {
            Request::Enqueue { items, client, .. } => {
                assert_eq!(items[0].title(), "电影");
                assert_eq!(items[0].direct_url.as_deref(), Some("http://host/x?api_key=k"));
                assert_eq!(client.token, "t");
            }
            _ => panic!("unexpected request"),
        }
    }

    #[test]
    fn response_serializes_state_and_errors() {
        let text = serde_json::to_string(&Response::state(Snapshot::default())).unwrap();
        assert!(text.contains("\"ok\":true"));
        assert!(text.contains("\"state\""));
        assert!(text.contains("\"protocol\":4"));

        let text = serde_json::to_string(&Response::err("boom")).unwrap();
        assert!(text.contains("\"ok\":false"));
        assert!(text.contains("boom"));
        assert!(!text.contains("\"state\""));
        assert!(!text.contains("\"error\":null"));
    }

    #[test]
    fn pause_requests_round_trip() {
        for req in [
            Request::PauseAll,
            Request::ResumeAll,
            Request::PauseServer {
                server: "srv".into(),
            },
            Request::ResumeServer {
                server: "srv".into(),
            },
        ] {
            let text = serde_json::to_string(&req).unwrap();
            let back: Request = serde_json::from_str(&text).unwrap();
            assert_eq!(text, serde_json::to_string(&back).unwrap());
        }
    }

    #[test]
    fn old_daemon_response_has_no_protocol_field() {
        // 0.0.3 的响应没有 protocol；交互模式据此判断需要重启后台服务。
        let old: Response = serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert_eq!(old.protocol, None);
        assert_ne!(old.protocol, Some(PROTOCOL_VERSION));
    }

    #[test]
    fn process_is_running_checks_proc_state() {
        assert!(process_is_running(std::process::id()));
        assert!(!process_is_running(u32::MAX));
    }
}
