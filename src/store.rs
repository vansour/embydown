//! 下载队列与下载记录的持久化。
//!
//! - `queue.json`：后台守护进程当前排队/运行中的任务。守护进程启动时据此恢复，
//!   正在运行的任务会重新排队，配合 `.part` 断点续传继续下载。
//! - `history.jsonl`：每次任务结束追加一行，只增不改（除非用户主动清空），
//!   TUI 异常退出、机器重启都不会丢。
//!
//! 所有文件都以 0600 权限原子写入：队列里可能带 `api_key` 的直链。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::config;
use crate::download::DownloadOpts;
use crate::emby::ClientCreds;
use crate::manager::Finished;
use crate::model::Item;

/// 队列文件格式版本。无法理解的版本一律当作空队列，避免用错格式恢复。
pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: u64,
    pub title: String,
    pub item: Item,
    pub opts: DownloadOpts,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedServer {
    /// 重建 Client 所需的连接参数（含 token）。
    #[serde(default)]
    pub creds: Option<ClientCreds>,
    #[serde(default)]
    pub max_jobs: usize,
    /// 用户暂停状态：恢复后保持暂停，直到手动继续。
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub queue: Vec<JobRecord>,
    /// 退出/崩溃时正在运行的任务；恢复时放回队列头部重新执行。
    #[serde(default)]
    pub active: Vec<JobRecord>,
    #[serde(default)]
    pub done: usize,
    #[serde(default)]
    pub failed: usize,
    #[serde(default)]
    pub cancelled: usize,
    #[serde(default)]
    pub skipped: usize,
    #[serde(default)]
    pub subs: usize,
    #[serde(default)]
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub servers: BTreeMap<String, PersistedServer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRecord {
    /// Unix 时间戳（秒）。
    #[serde(default)]
    pub time_unix: u64,
    #[serde(flatten)]
    pub entry: Finished,
}

pub fn state_path() -> Result<PathBuf> {
    Ok(config::dir()?.join("queue.json"))
}

pub fn history_path() -> Result<PathBuf> {
    Ok(config::dir()?.join("history.jsonl"))
}

pub fn log_path() -> Result<PathBuf> {
    Ok(config::dir()?.join("daemon.log"))
}

/// 守护进程 socket。优先放在 `XDG_RUNTIME_DIR`（路径短、重启后自动清理）。
pub fn socket_path() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty());
    let dir = match runtime {
        Some(dir) => dir.join("embydown"),
        None => config::dir()?,
    };
    let path = dir.join("daemon.sock");
    // sun_path 上限通常为 108 字节。
    if path.as_os_str().len() > 100 {
        anyhow::bail!(
            "后台服务 socket 路径过长（{}），请设置较短的 XDG_RUNTIME_DIR",
            path.display()
        );
    }
    Ok(path)
}

/// 单实例锁文件，防止同时启动两个守护进程重复下载同一批文件。
pub fn lock_path() -> Result<PathBuf> {
    Ok(socket_path()?
        .parent()
        .map(|p| p.join("daemon.lock"))
        .unwrap_or_else(|| PathBuf::from("daemon.lock")))
}

/// 读取队列；文件不存在或损坏时返回空队列（守护进程必须能继续启动）。
pub fn load_state() -> Option<PersistedState> {
    match state_path() {
        Ok(path) => load_state_from(&path),
        Err(e) => {
            eprintln!("⚠️  无法确定队列文件位置: {e:#}");
            None
        }
    }
}

pub fn load_state_from(path: &Path) -> Option<PersistedState> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("⚠️  读取队列文件 {} 失败: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_str::<PersistedState>(&text) {
        Ok(mut state) if state.version == STATE_VERSION => {
            state.version = STATE_VERSION;
            Some(state)
        }
        Ok(state) => {
            eprintln!(
                "⚠️  队列文件版本 {} 无法识别（当前 {}），本次不恢复旧队列",
                state.version, STATE_VERSION
            );
            None
        }
        Err(e) => {
            eprintln!("⚠️  队列文件 {} 已损坏: {e}", path.display());
            None
        }
    }
}

pub fn save_state(state: &PersistedState) -> Result<()> {
    let path = state_path()?;
    save_state_to(&path, state)
}

pub fn save_state_to(path: &Path, state: &PersistedState) -> Result<()> {
    let mut text = serde_json::to_vec_pretty(state).context("序列化下载队列失败")?;
    text.push(b'\n');
    config::write_atomic_bytes(path, &text)
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 追加一条完成记录并落盘。历史文件只追加，不做整文件重写。
pub fn append_history(entry: &Finished) -> Result<()> {
    let path = history_path()?;
    append_history_to(&path, entry, now_unix())
}

pub fn append_history_to(path: &Path, entry: &Finished, time_unix: u64) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建目录失败: {}", dir.display()))?;
    }
    let record = HistoryRecord {
        time_unix,
        entry: entry.clone(),
    };
    let mut line = serde_json::to_vec(&record).context("序列化下载记录失败")?;
    line.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("打开下载记录 {} 失败", path.display()))?;
    file.write_all(&line)
        .with_context(|| format!("写入下载记录 {} 失败", path.display()))?;
    // 记录是“永久保存”的关键，写完立刻刷到磁盘。
    file.sync_data().ok();
    Ok(())
}

/// 读取全部历史记录；单行损坏只跳过该行。
pub fn load_history() -> Vec<HistoryRecord> {
    match history_path() {
        Ok(path) => load_history_from(&path),
        Err(_) => Vec::new(),
    }
}

pub fn load_history_from(path: &Path) -> Vec<HistoryRecord> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            eprintln!("⚠️  读取下载记录 {} 失败: {e}", path.display());
            return Vec::new();
        }
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<HistoryRecord>(line).ok())
        .collect()
}

/// 用户主动清空历史时调用。
pub fn clear_history() -> Result<()> {
    let path = history_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("删除下载记录 {} 失败", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::{Finished, FinishedStatus};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "embydown-store-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn job(id: u64) -> JobRecord {
        JobRecord {
            id,
            title: format!("任务 {id}"),
            item: Item {
                id: format!("item{id}"),
                name: Some(format!("第 {id} 集")),
                item_type: Some("Episode".into()),
                direct_url: Some(format!("http://host/Videos/{id}/stream?api_key=secret")),
                source_locked: true,
                version: Some("1080p".into()),
                ..Default::default()
            },
            opts: DownloadOpts::default(),
        }
    }

    #[test]
    fn queue_state_round_trips_with_private_fields() {
        let dir = temp_dir("queue");
        let path = dir.join("queue.json");
        let mut state = PersistedState {
            version: STATE_VERSION,
            seq: 7,
            ..Default::default()
        };
        state.servers.insert(
            "srv".into(),
            PersistedServer {
                creds: Some(ClientCreds {
                    server: "http://host:8096".into(),
                    token: "tok".into(),
                    user_id: "u1".into(),
                    insecure: true,
                }),
                max_jobs: 3,
                paused: true,
                queue: vec![job(8)],
                active: vec![job(9)],
                done: 1,
                bytes: 42,
                ..Default::default()
            },
        );
        save_state_to(&path, &state).unwrap();

        let back = load_state_from(&path).unwrap();
        assert_eq!(back.seq, 7);
        let srv = &back.servers["srv"];
        assert_eq!(srv.queue[0].item.direct_url.as_deref(), Some("http://host/Videos/8/stream?api_key=secret"));
        assert_eq!(srv.queue[0].item.version.as_deref(), Some("1080p"));
        assert!(srv.queue[0].item.source_locked);
        assert_eq!(srv.active[0].id, 9);
        assert_eq!(srv.bytes, 42);
        assert!(srv.paused);
        assert!(srv.creds.as_ref().unwrap().insecure);

        // 队列里可能有 api_key，权限必须是 0600。
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_or_unknown_queue_is_not_restored() {
        let dir = temp_dir("corrupt");
        let path = dir.join("queue.json");

        std::fs::write(&path, b"{not json").unwrap();
        assert!(load_state_from(&path).is_none());

        std::fs::write(&path, br#"{"version":999,"servers":{}}"#).unwrap();
        assert!(load_state_from(&path).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn history_is_append_only_and_tolerates_bad_lines() {
        let dir = temp_dir("history");
        let path = dir.join("history.jsonl");
        let entry = Finished {
            server: "srv".into(),
            title: "电影".into(),
            status: FinishedStatus::Done,
            bytes: 123,
            error: None,
        };
        append_history_to(&path, &entry, 1_700_000_000).unwrap();
        append_history_to(&path, &entry, 1_700_000_001).unwrap();

        // 模拟写入中断留下的半行。
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"time_unix\":12").unwrap();
        drop(file);

        let records = load_history_from(&path);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].time_unix, 1_700_000_000);
        assert_eq!(records[0].entry.title, "电影");
        std::fs::remove_dir_all(&dir).ok();
    }
}
