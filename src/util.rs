//! 小工具：日志、字节格式化、限速解析、文件名清洗。

use anyhow::{Context, Result};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

static VERBOSE: AtomicU8 = AtomicU8::new(0);

pub fn set_verbose(v: u8) {
    VERBOSE.store(v, Ordering::Relaxed);
}

pub fn verbose() -> u8 {
    VERBOSE.load(Ordering::Relaxed)
}

/// 简单进度日志：level 越大越吵（0 = 只报错，1 = 常规，2 = 调试）。
pub fn vlog(level: u8, msg: impl AsRef<str>) {
    if verbose() >= level {
        eprintln!("{}", msg.as_ref());
    }
}

pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if n == 0 {
        return "0 B".to_string();
    }
    let mut val = n as f64;
    let mut idx = 0;
    while val >= 1024.0 && idx < UNITS.len() - 1 {
        val /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{} {}", n, UNITS[0])
    } else {
        format!("{:.2} {}", val, UNITS[idx])
    }
}

/// 解析 `--limit-rate`：支持 `5M`、`800K`、`1048576`（1024 进制）。
pub fn parse_rate(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.chars().last().unwrap().to_ascii_lowercase() {
        'k' => (&s[..s.len() - 1], 1024u64),
        'm' => (&s[..s.len() - 1], 1024 * 1024),
        'g' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    let v: f64 = num.trim().parse().ok()?;
    if !v.is_finite() || v <= 0.0 {
        return None;
    }
    let bytes = v * mult as f64;
    // 小于 1 字节/秒的值（如 0.5）截断后会变成 0，导致限速器除零 panic。
    if !bytes.is_finite() || bytes < 1.0 {
        return None;
    }
    Some(bytes as u64)
}

/// Emby 的 RunTimeTicks 是 100ns 单位。
pub fn format_ticks(ticks: i64) -> String {
    let secs = ticks / 10_000_000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{:02}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

/// 清洗路径中的单个组件：去掉非法字符与控制字符。
pub fn sanitize_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => out.push('_'),
            c if (c as u32) < 0x20 => out.push('_'),
            c => out.push(c),
        }
    }
    let trimmed = out.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 清洗整条相对路径（以 `/` 分段，逐段 sanitize）。
pub fn sanitize_path(rel: &str) -> String {
    rel.split('/')
        .filter(|seg| !seg.trim().is_empty())
        .map(sanitize_component)
        .collect::<Vec<_>>()
        .join("/")
}

/// 截断过长的单段文件名（按 UTF-8 **字节**算，Linux NAME_MAX=255）。
/// 注意中文 1 个字 3 字节，按字符截断仍会 ENAMETOOLONG。
pub fn truncate_component(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].trim_end().to_string()
}

/// 展开路径开头的 `~`（仅 `~` 和 `~/...`）。
///
/// clap/PathBuf 不会展开 shell 的波浪号，`-o "~/Downloads"` 会真的在当前
/// 目录下创建一个名为 `~` 的文件夹。这里统一在真正使用路径前展开。
pub fn expand_tilde(path: &Path) -> Result<PathBuf> {
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(first)) if first == "~" => {
            let home = std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(PathBuf::from)
                .context("无法展开 ~：HOME 未设置")?;
            let rest = components.as_path();
            Ok(if rest.as_os_str().is_empty() {
                home
            } else {
                home.join(rest)
            })
        }
        _ => Ok(path.to_path_buf()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rate_rejects_values_that_round_to_zero() {
        assert_eq!(parse_rate("0.5"), None);
        assert_eq!(parse_rate("0.0001"), None);
        assert_eq!(parse_rate("0"), None);
        assert_eq!(parse_rate("NaN"), None);
        assert_eq!(parse_rate("inf"), None);
    }

    #[test]
    fn parse_rate_units() {
        assert_eq!(parse_rate("5M"), Some(5 * 1024 * 1024));
        assert_eq!(parse_rate("800K"), Some(800 * 1024));
        assert_eq!(parse_rate("1024"), Some(1024));
    }

    #[test]
    fn tilde_expands_to_home() {
        let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
            return;
        };
        let home = PathBuf::from(home);
        assert_eq!(expand_tilde(Path::new("~")).unwrap(), home);
        assert_eq!(
            expand_tilde(Path::new("~/Movies/anime")).unwrap(),
            home.join("Movies/anime")
        );
        assert_eq!(expand_tilde(Path::new("/abs/path")).unwrap(), PathBuf::from("/abs/path"));
        assert_eq!(expand_tilde(Path::new("./relative")).unwrap(), PathBuf::from("./relative"));
        // ~user 不做展开（避免猜错用户）
        assert_eq!(expand_tilde(Path::new("~user/x")).unwrap(), PathBuf::from("~user/x"));
    }
}
