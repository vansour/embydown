//! 自更新：从 GitHub Releases 下载当前架构的裸二进制，校验 SHA-256 后原子替换。
//!
//! 只依赖发布包里的裸二进制资产（`embydown-vX.Y.Z-x86_64-unknown-linux-gnu.bin`），
//! 不需要 curl、tar 或外部解压工具。替换前会把新文件写到可执行文件同目录的临时
//! 文件，校验通过后再 rename，避免写坏正在使用的二进制。

use std::cmp::Ordering;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use serde_json::Value;
use sha2::{Digest, Sha256};

const REPO: &str = "vansour/embydown";
const TARGET: &str = "x86_64-unknown-linux-gnu";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// `embydown update`：检查并安装最新版或指定版本。
pub async fn run(check: bool, version: Option<String>, force: bool) -> Result<()> {
    ensure_supported_platform()?;

    let explicit = version
        .as_deref()
        .map(|v| !v.trim().is_empty() && !v.eq_ignore_ascii_case("latest"))
        .unwrap_or(false);
    let client = build_client()?;
    let tag = if explicit {
        normalize_tag(version.as_deref().unwrap_or_default())
    } else {
        latest_tag(&client).await?
    };
    let online = tag.trim_start_matches('v').to_string();

    println!("当前版本：v{CURRENT}");
    println!("线上版本：{tag}");

    if !force {
        if !explicit {
            match compare_versions(&online, CURRENT) {
                Some(Ordering::Less) => {
                    println!("当前版本比线上版本新，无需更新（用 --force 可重新安装线上版本）");
                    return Ok(());
                }
                Some(Ordering::Equal) => {
                    println!("✅ 已是最新版本");
                    return Ok(());
                }
                _ => {}
            }
        } else if online == CURRENT {
            println!("✅ 当前已经是 {tag}（用 --force 可重新安装）");
            return Ok(());
        }
    }

    if check {
        if online == CURRENT {
            println!("✅ 当前已是 {tag}（--check 不安装；用 --force 可重新安装）");
        } else {
            println!("可更新：v{CURRENT} → {tag}");
        }
        return Ok(());
    }

    let asset = format!("embydown-{tag}-{TARGET}.bin");
    let base = release_base(&tag);
    let asset_url = format!("{base}/{asset}");
    let checksum_url = format!("{asset_url}.sha256");

    let exe = std::env::current_exe().context("无法定位当前可执行文件")?;
    let dir = exe
        .parent()
        .context("无法确定可执行文件所在目录")?
        .to_path_buf();
    let temp = dir.join(format!(
        ".{}.update-{}",
        exe.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("embydown"),
        std::process::id()
    ));

    println!("↓ 正在下载 {asset}");
    let result = async {
        let expected = fetch_checksum(&client, &checksum_url).await?;
        let actual = download_binary(&client, &asset_url, &temp, &exe).await?;
        if actual != expected {
            bail!("SHA-256 校验失败：期望 {expected}，实际 {actual}。已放弃安装");
        }
        println!("✓ SHA-256 校验通过");
        install_binary(&temp, &exe)?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Err(e) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(e);
    }

    println!("✅ 已更新到 {tag}");
    match crate::daemon::stop_running_daemon().await {
        Ok(true) => {
            println!("🔄 已停止旧后台服务（队列已保存，下次运行 embydown 时自动续传）");
        }
        Ok(false) => {}
        Err(e) => {
            println!("⚠️  未能停止旧后台服务：{e:#}");
            println!("   它仍在使用旧版本；可手动结束后重试。");
        }
    }
    println!("重新运行 embydown 即可使用新版本。");
    Ok(())
}

fn ensure_supported_platform() -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("自更新目前只支持 Linux");
    }
    if !cfg!(target_arch = "x86_64") || !cfg!(target_env = "gnu") {
        bail!(
            "当前构建不是 x86_64 + glibc，官方预编译包无法直接替换；\
             请从源码构建：cargo build --release"
        );
    }
    Ok(())
}

fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("embydown/{CURRENT}"))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .build()
        .context("创建 HTTP 客户端失败")
}

/// 读取最新正式版的标签；GitHub API 不可用时退回解析 latest 重定向。
async fn latest_tag(client: &reqwest::Client) -> Result<String> {
    match api_latest_tag(client).await {
        Ok(tag) => Ok(tag),
        Err(api_error) => match redirect_latest_tag(client).await {
            Ok(tag) => Ok(tag),
            Err(_) => Err(api_error),
        },
    }
}

async fn api_latest_tag(client: &reqwest::Client) -> Result<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let response = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("请求 GitHub 最新版本失败")?;
    if !response.status().is_success() {
        bail!(
            "GitHub API 返回 HTTP {}（可能触发了限流；可稍后重试，或用 --version 指定版本）",
            response.status()
        );
    }
    let json: Value = response.json().await.context("解析 GitHub 响应失败")?;
    let tag = json
        .get("tag_name")
        .and_then(Value::as_str)
        .context("GitHub 响应里没有 tag_name")?;
    Ok(normalize_tag(tag))
}

async fn redirect_latest_tag(client: &reqwest::Client) -> Result<String> {
    let url = format!("https://github.com/{REPO}/releases/latest");
    let response = client
        .get(&url)
        .send()
        .await
        .context("请求 GitHub Releases 失败")?;
    let final_url = response.url().as_str();
    let tag = final_url
        .rsplit('/')
        .next()
        .filter(|tag| tag.starts_with('v'))
        .context("无法从 GitHub 跳转地址解析版本号")?;
    Ok(normalize_tag(tag))
}

async fn fetch_checksum(client: &reqwest::Client, url: &str) -> Result<String> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("下载校验文件失败：{url}"))?;
    if !response.status().is_success() {
        bail!(
            "下载校验文件失败：HTTP {}（{url}）\n\
             该版本可能没有自更新资产，请改用 install.sh 安装。",
            response.status()
        );
    }
    let text = response.text().await.context("读取校验文件失败")?;
    parse_checksum(&text)
}

fn parse_checksum(text: &str) -> Result<String> {
    let hash = text
        .split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
        .context("校验文件格式无效")?;
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!(
            "校验文件格式无效：{:?}",
            text.lines().next().unwrap_or(text)
        );
    }
    Ok(hash)
}

/// 流式下载到同目录临时文件，同时计算 SHA-256。
async fn download_binary(
    client: &reqwest::Client,
    url: &str,
    temp: &Path,
    exe: &Path,
) -> Result<String> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("下载失败：{url}"))?;
    if !response.status().is_success() {
        bail!("下载新版本失败：HTTP {}（{url}）", response.status());
    }

    let total = response.content_length().unwrap_or(0);
    let bar = ProgressBar::new(total);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} {bytes}/{total_bytes} {bar:32.cyan/blue} {percent}%",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar()),
    );

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(temp)
        .with_context(|| {
            format!(
                "无法写入 {}。需要对该目录有写权限；\
                 若安装在 /usr/local/bin，请用 sudo 运行 embydown update。",
                exe.parent().unwrap_or(exe).display()
            )
        })?;

    let mut hasher = Sha256::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("下载数据流中断")?;
        hasher.update(&chunk);
        file.write_all(&chunk).context("写入临时文件失败")?;
        bar.inc(chunk.len() as u64);
    }
    file.sync_all().context("同步临时文件失败")?;
    bar.finish_and_clear();
    Ok(format!("{:x}", hasher.finalize()))
}

fn install_binary(temp: &Path, exe: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(temp, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("设置可执行权限失败：{}", temp.display()))?;
    std::fs::rename(temp, exe)
        .with_context(|| format!("替换 {} 失败（可尝试 sudo 后重试）", exe.display()))?;
    Ok(())
}

/// Release 下载目录。`EMBYDOWN_RELEASE_BASE` 可指向镜像
/// （例如 `https://mirror.example.com/embydown/releases/download`）。
fn release_base(tag: &str) -> String {
    match std::env::var("EMBYDOWN_RELEASE_BASE") {
        Ok(base) if !base.trim().is_empty() => {
            format!("{}/{tag}", base.trim().trim_end_matches('/'))
        }
        _ => format!("https://github.com/{REPO}/releases/download/{tag}"),
    }
}

fn normalize_tag(input: &str) -> String {
    let tag = input.trim();
    if tag.starts_with('v') || tag.starts_with('V') {
        format!("v{}", &tag[1..])
    } else {
        format!("v{tag}")
    }
}

/// 解析 `X.Y.Z`（允许 `v` 前缀和 `-rc1` 之类的后缀）；解析失败返回 None。
fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.trim().trim_start_matches(['v', 'V']);
    let core = value.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    Some(parse_version(a)?.cmp(&parse_version(b)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_tags() {
        assert_eq!(normalize_tag("0.0.5"), "v0.0.5");
        assert_eq!(normalize_tag("v0.0.5"), "v0.0.5");
        assert_eq!(normalize_tag(" V1.2.3 "), "v1.2.3");
    }

    #[test]
    fn parses_and_compares_versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_version("not-a-version"), None);
        assert_eq!(compare_versions("0.0.5", "0.0.4"), Some(Ordering::Greater));
        assert_eq!(compare_versions("v0.0.4", "0.0.4"), Some(Ordering::Equal));
    }

    #[test]
    fn parses_sha256sum_files() {
        let hash = "a".repeat(64);
        let text = format!("{hash}  embydown-x86_64-unknown-linux-gnu\n");
        assert_eq!(parse_checksum(&text).unwrap(), hash);
        assert!(parse_checksum("not-a-hash  file").is_err());
    }

    #[test]
    fn replaces_binary_atomically() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "embydown-update-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("embydown");
        let temp = dir.join(".embydown.update");
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&temp, b"new").unwrap();

        install_binary(&temp, &exe).unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert!(!temp.exists());
        let mode = std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
