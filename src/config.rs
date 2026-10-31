//! 多服务器登录配置：保存在 `$XDG_CONFIG_HOME/embydown/config.json`（Linux）。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 同一进程内序列化写入（交互模式登录/删除 + 后台任务可能同时保存）。
static SAVE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub server: String,
    pub token: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub insecure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 交互模式「下载设置」的默认值；修改后自动保存。
    /// 临时文件夹：下载先落到这里。
    /// 旧配置里的 `out` 键会自动迁移到这个字段。
    #[serde(alias = "out")]
    pub temp_dir: String,
    /// 最终文件夹：审查通过后用 `embydown sync` 复制到这里。
    pub final_dir: String,
    pub connections: usize,
    pub jobs: usize,
    pub limit: String,
    pub subs: bool,
    pub overwrite: bool,
    pub resume: bool,
    pub pattern: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            temp_dir: "./downloads".into(),
            final_dir: "./library".into(),
            connections: 2,
            jobs: 2,
            limit: String::new(),
            subs: true,
            overwrite: false,
            resume: true,
            pattern: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// 别名 → 服务器配置。BTreeMap 保证输出顺序稳定。
    #[serde(default)]
    pub servers: BTreeMap<String, Profile>,
    /// 交互模式下载设置（输出目录、并发、限速等），与登录信息分开保存。
    #[serde(default)]
    pub settings: Settings,
}

impl Config {
    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.servers.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.servers.keys().map(String::as_str).collect()
    }

    pub fn name_list(&self) -> String {
        if self.servers.is_empty() {
            "（无）".to_string()
        } else {
            self.names().join("、")
        }
    }

    /// 没有 --server 时：只有一个服务器就直接用，否则提示用户指定。
    pub fn only(&self) -> Result<(&str, &Profile)> {
        match self.servers.len() {
            0 => bail!(
                "尚未登录任何服务器；请先运行 embydown login --server <URL> --user <账号> --password <密码>"
            ),
            1 => {
                let (name, profile) = self.servers.iter().next().unwrap();
                Ok((name.as_str(), profile))
            }
            _ => bail!(
                "已登录多个服务器（{}），请用 --server <名称> 指定；用 embydown servers 查看",
                self.name_list()
            ),
        }
    }

    /// 按名称筛选；未指定时返回全部（按名称排序）。名称不存在时给出提示。
    pub fn select(&self, name: Option<&str>) -> Result<Vec<(String, Profile)>> {
        match name {
            Some(n) => match self.servers.get(n) {
                Some(p) => Ok(vec![(n.to_string(), p.clone())]),
                None => bail!(
                    "没有名为 {n} 的服务器；已登录：{}（用 embydown servers 查看）",
                    self.name_list()
                ),
            },
            None => {
                if self.servers.is_empty() {
                    bail!("尚未登录任何服务器；请先运行 embydown login --server <URL> --user <账号> --password <密码>");
                }
                Ok(self.servers.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            }
        }
    }
}

/// 从服务器地址推导默认别名，如 `https://media.micu.hk` → `media.micu.hk`。
pub fn name_from_server(server: &str) -> String {
    match reqwest::Url::parse(server) {
        Ok(u) => match (u.host_str(), u.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => server.trim_end_matches('/').to_string(),
        },
        Err(_) => server.trim_end_matches('/').to_string(),
    }
}

pub fn path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("无法确定配置目录（HOME / XDG_CONFIG_HOME 均未设置）")?;
    Ok(base.join("embydown").join("config.json"))
}

/// 配置/下载记录所在目录。
pub fn dir() -> Result<PathBuf> {
    Ok(path()?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".")))
}

/// 读取配置；文件不存在时返回 `None`（供 login 首次使用）。
pub fn load_optional() -> Result<Option<Config>> {
    let path = path()?;
    let _guard = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    load_from(&path)
}

fn load_from(path: &Path) -> Result<Option<Config>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("读取配置失败: {}", path.display())),
    };

    let value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("解析配置失败: {}", path.display()))?;
    if value.get("servers").is_some() {
        let cfg: Config = serde_json::from_value(value)
            .with_context(|| format!("解析配置失败: {}", path.display()))?;
        return Ok(Some(cfg));
    }

    // 兼容旧版单服务器配置
    let flat: FlatConfig = serde_json::from_value(value)
        .with_context(|| format!("解析配置失败: {}", path.display()))?;
    let name = name_from_server(&flat.server);
    let mut cfg = Config::default();
    cfg.servers.insert(
        name,
        Profile {
            server: flat.server,
            token: flat.token,
            user_id: flat.user_id,
            username: flat.username,
            insecure: flat.insecure,
        },
    );
    write_atomic(path, &cfg)?;
    Ok(Some(cfg))
}

/// 原子地“读取 → 修改 → 保存”。
///
/// load 和 save 之间如果被另一个进程（交互模式与 CLI 同时登录/删除）插入写入，
/// 后保存的进程会用旧快照覆盖掉对方的改动。这里同时使用进程内互斥锁和
/// 锁文件把整个读改写过程串行化。
pub fn edit<T>(f: impl FnOnce(&mut Config) -> Result<T>) -> Result<T> {
    let path = path()?;
    let _guard = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)
        .with_context(|| format!("创建配置目录失败: {}", dir.display()))?;
    let _file_lock = acquire_file_lock(dir);
    let mut cfg = load_from(&path)?.unwrap_or_default();
    let out = f(&mut cfg)?;
    write_atomic(&path, &cfg)?;
    Ok(out)
}

fn write_atomic(path: &Path, cfg: &Config) -> Result<()> {
    let mut text = serde_json::to_vec_pretty(cfg)?;
    text.push(b'\n');
    write_atomic_bytes(path, &text)
}

/// 以 0600 权限原子写文件：临时文件 + fsync + rename。
///
/// 原地 truncate 时断电/磁盘满会留下空或半截文件，之后所有命令
/// （包括 login）都会“解析配置失败”。队列与历史记录也复用这里。
pub(crate) fn write_atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建目录失败: {}", dir.display()))?;
    }
    let tmp = tmp_path(path);
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("写入文件失败: {}", tmp.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        // 已存在的文件可能权限更宽，统一收紧到 0600
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).ok();
        std::fs::rename(&tmp, path)
            .with_context(|| format!("保存文件失败: {}", path.display()))?;
        // 让 rename 本身也落盘（目录项）。
        if let Some(dir) = path.parent()
            && let Ok(d) = std::fs::File::open(dir)
        {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}", std::process::id()));
    path.with_file_name(name)
}

/// 跨进程锁：用 `create_new` 原子创建锁文件，内容写 PID；进程崩溃后
/// 通过 `/proc/<pid>` 判断锁是否已过期。
struct FileLock {
    path: Option<PathBuf>,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn acquire_file_lock(dir: &Path) -> FileLock {
    let lock = dir.join(".config.json.lock");
    // 最多等约 1.5 秒；拿不到也继续（原子 rename 仍能保证不写坏文件）。
    for _ in 0..30 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(mut f) => {
                let _ = write!(f, "{}", std::process::id());
                return FileLock {
                    path: Some(lock),
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_is_stale(&lock) {
                    let _ = std::fs::remove_file(&lock);
                    continue;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
    FileLock { path: None }
}

fn lock_is_stale(path: &Path) -> bool {
    // 只在有 /proc 的系统（Linux）上判断，避免在 macOS 等平台把活锁当成过期锁删掉。
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

#[derive(Debug, Deserialize)]
struct FlatConfig {
    server: String,
    token: String,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    insecure: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "embydown-test-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_write_round_trips_and_leaves_no_temp_file() {
        let dir = temp_dir("cfg");
        let path = dir.join("config.json");
        let mut cfg = Config::default();
        cfg.servers.insert(
            "srv".into(),
            Profile {
                server: "http://example:8096".into(),
                token: "secret".into(),
                user_id: "u1".into(),
                username: Some("alice".into()),
                insecure: false,
            },
        );
        write_atomic(&path, &cfg).unwrap();

        let loaded = load_from(&path).unwrap().unwrap();
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.servers["srv"].token, "secret");

        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "config.json")
            .collect();
        assert!(leftovers.is_empty(), "临时文件未清理: {leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn settings_round_trip_and_defaults_for_legacy_config() {
        let mut cfg = Config::default();
        cfg.settings.temp_dir = "/media/downloads".into();
        cfg.settings.jobs = 4;
        cfg.settings.subs = false;
        let text = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.settings.temp_dir, "/media/downloads");
        assert_eq!(back.settings.jobs, 4);
        assert!(!back.settings.subs);

        // 旧配置文件（只有 servers）读出来时，设置回落到默认值。
        let legacy: Config = serde_json::from_str(r#"{"servers":{}}"#).unwrap();
        assert_eq!(legacy.settings.temp_dir, "./downloads");
        assert_eq!(legacy.settings.connections, 2);
        assert_eq!(legacy.settings.jobs, 2);
        assert!(legacy.settings.resume);

        // 旧配置里的 out 键迁移到 temp_dir。
        let old_key: Config =
            serde_json::from_str(r#"{"settings":{"out":"/old/downloads"}}"#).unwrap();
        assert_eq!(old_key.settings.temp_dir, "/old/downloads");
        assert_eq!(old_key.settings.final_dir, "./library");
    }

    #[test]
    fn corrupt_config_is_reported_not_silently_overwritten() {
        let dir = temp_dir("corrupt");
        let path = dir.join("config.json");
        std::fs::write(&path, b"{\"servers\":").unwrap();
        assert!(load_from(&path).is_err());
        // 坏文件应该保持原样，供用户手工恢复。
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"servers\":");
        std::fs::remove_dir_all(&dir).ok();
    }
}
