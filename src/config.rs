//! 多服务器登录配置：保存在 `$XDG_CONFIG_HOME/embydown/config.json`（Linux）。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// 别名 → 服务器配置。BTreeMap 保证输出顺序稳定。
    #[serde(default)]
    pub servers: BTreeMap<String, Profile>,
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

/// 读取配置；文件不存在时返回 `None`（供 login 首次使用）。
pub fn load_optional() -> Result<Option<Config>> {
    let path = path()?;
    let text = match std::fs::read_to_string(&path) {
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
    save_to(&path, &cfg)?;
    Ok(Some(cfg))
}

pub fn load() -> Result<Config> {
    match load_optional()? {
        Some(cfg) if !cfg.servers.is_empty() => Ok(cfg),
        _ => bail!(
            "尚未登录任何服务器；请先运行 embydown login --server <URL> --user <账号> --password <密码>"
        ),
    }
}

pub fn save(cfg: &Config) -> Result<PathBuf> {
    let path = path()?;
    save_to(&path, cfg)?;
    Ok(path)
}

fn save_to(path: &PathBuf, cfg: &Config) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建配置目录失败: {}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(cfg)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("写入配置失败: {}", path.display()))?;
    file.write_all(text.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;
    // 已存在的文件可能权限更宽，统一收紧到 0600
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
    Ok(())
}

/// 删除整个配置文件。
pub fn clear() -> Result<bool> {
    let path = path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("删除配置失败: {}", path.display())),
    }
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
