//! 小工具：日志、字节格式化、限速解析、文件名清洗。

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
    if v <= 0.0 {
        return None;
    }
    Some((v * mult as f64) as u64)
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
