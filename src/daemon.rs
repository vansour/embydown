//! 后台下载守护进程。
//!
//! TUI 不再自己执行下载：它只通过 `$XDG_RUNTIME_DIR/embydown/daemon.sock`
//! 发送「入队 / 取消 / 查询」请求。守护进程是独立的 setsid 子进程，所以：
//! - TUI 崩溃、Ctrl+C、直接关掉终端，下载都不受影响；
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
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::SignalKind;
use tokio::sync::mpsc;

use crate::download::DownloadOpts;
use crate::emby::{Client, ClientCreds};
use crate::manager::{Manager, Snapshot};
use crate::model::Item;
use crate::store;

// ───────────────────────── 协议 ─────────────────────────

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Enqueue {
        server: String,
        client: ClientCreds,
        items: Vec<Item>,
        opts: DownloadOpts,
    },
    CancelServer {
        server: String,
    },
    CancelAll,
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
}

impl Response {
    fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            state: None,
        }
    }

    fn state(state: Snapshot) -> Self {
        Self {
            ok: true,
            error: None,
            state: Some(state),
        }
    }

    fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            state: None,
        }
    }
}

// ───────────────────────── 守护进程 ─────────────────────────

/// 运行后台下载服务（`embydown daemon`），通常由 TUI 自动拉起。
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
                manager.enqueue(server, client, items, opts);
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
    !Path::new(&format!("/proc/{pid}")).exists()
}

// ───────────────────────── 界面侧客户端 ─────────────────────────

/// TUI 持有的后台服务句柄：缓存最新快照，后台任务负责连接/重连。
pub struct DaemonHandle {
    snapshot: Arc<Mutex<Snapshot>>,
    error: Arc<Mutex<Option<String>>>,
    tx: mpsc::UnboundedSender<Request>,
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

    /// 取出并清空最近一次后台通信错误（由 TUI 状态栏显示）。
    pub fn take_error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn send(&self, req: Request) {
        let _ = self.tx.send(req);
    }

    pub fn enqueue(&self, server: String, client: Client, items: Vec<Item>, opts: DownloadOpts) {
        self.send(Request::Enqueue {
            server,
            client: client.creds(),
            items,
            opts,
        });
    }

    pub fn cancel_server(&self, server: &str) {
        self.send(Request::CancelServer {
            server: server.to_string(),
        });
    }

    pub fn cancel_all(&self) {
        self.send(Request::CancelAll);
    }

    pub fn clear_finished(&self) {
        self.send(Request::ClearHistory);
    }
}

async fn connection_task(
    mut rx: mpsc::UnboundedReceiver<Request>,
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

async fn run_connection(
    rx: &mut mpsc::UnboundedReceiver<Request>,
    snapshot: &Arc<Mutex<Snapshot>>,
) -> Result<()> {
    ensure_daemon().await?;
    let socket = store::socket_path()?;
    let stream = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("连接 {} 失败", socket.display()))?;
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // 定期拉取快照；界面发来的命令优先处理。
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let request = tokio::select! {
            _ = tick.tick() => Request::Snapshot,
            req = rx.recv() => req.context("界面已退出")?,
        };
        let mut out = serde_json::to_vec(&request).context("序列化请求失败")?;
        out.push(b'\n');
        writer
            .write_all(&out)
            .await
            .context("发送请求到后台服务失败")?;
        writer.flush().await.ok();

        let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .context("后台下载服务响应超时")??
            .context("后台下载服务断开连接")?;
        let resp: Response = serde_json::from_str(&line).context("解析后台响应失败")?;
        if let Some(state) = resp.state {
            *snapshot.lock().unwrap_or_else(|e| e.into_inner()) = state;
        }
        if !resp.ok {
            bail!(resp.error.unwrap_or_else(|| "后台服务返回未知错误".into()));
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

/// 以独立进程组启动 `embydown daemon`：不随 TUI 一起被 Ctrl+C 结束。
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
            opts: DownloadOpts::default(),
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

        let text = serde_json::to_string(&Response::err("boom")).unwrap();
        assert!(text.contains("\"ok\":false"));
        assert!(text.contains("boom"));
        assert!(!text.contains("\"state\""));
        assert!(!text.contains("\"error\":null"));
    }
}
