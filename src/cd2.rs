//! 极简 CloudDrive2 gRPC 客户端：只用于读取上传任务列表。
//!
//! CD2 的挂载写入是异步的：FUSE 写入先落到本地缓存，上传在后台进行。
//! 因此不能通过写入耗时判断秒传，只能查询 CD2 的上传任务状态。
//! 这里手写 protobuf 编解码，避免引入完整的 gRPC 依赖。

use std::time::Duration;

use anyhow::{Context, Result, bail};

/// CD2 上传任务的状态枚举（与 clouddrive.proto 保持一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadStatus {
    WaitForPreprocessing,
    Preprocessing,
    Cancelled,
    Transfer,
    Pause,
    Finish,
    Skipped,
    InQueue,
    Ignored,
    Error,
    FatalError,
    Unknown,
}

impl UploadStatus {
    pub fn from_code(code: i32) -> Self {
        match code {
            0 => Self::WaitForPreprocessing,
            1 => Self::Preprocessing,
            2 => Self::Cancelled,
            3 => Self::Transfer,
            4 => Self::Pause,
            5 => Self::Finish,
            6 => Self::Skipped,
            7 => Self::InQueue,
            8 => Self::Ignored,
            9 => Self::Error,
            10 => Self::FatalError,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::WaitForPreprocessing => "等待预处理",
            Self::Preprocessing => "预处理中",
            Self::Cancelled => "已取消",
            Self::Transfer => "上传中",
            Self::Pause => "已暂停",
            Self::Finish => "完成",
            Self::Skipped => "跳过（秒传）",
            Self::InQueue => "排队中",
            Self::Ignored => "已忽略",
            Self::Error => "出错",
            Self::FatalError => "严重错误",
            Self::Unknown => "未知",
        }
    }

    pub fn is_finished(self) -> bool {
        matches!(self, Self::Finish | Self::Skipped)
    }

    pub fn is_failed(self) -> bool {
        matches!(
            self,
            Self::Cancelled | Self::Error | Self::FatalError | Self::Ignored
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct UploadTask {
    pub key: String,
    pub dest_path: String,
    pub size: u64,
    pub transferred_bytes: u64,
    pub status: String,
    pub error: String,
    pub operator_type: i32,
    pub status_enum: i32,
}

impl UploadTask {
    pub fn status(&self) -> UploadStatus {
        if self.status_enum != 0 {
            UploadStatus::from_code(self.status_enum)
        } else if self.status.is_empty() {
            UploadStatus::Unknown
        } else {
            UploadStatus::from_code(parse_status_text(&self.status))
        }
    }
}

/// CD2 gRPC 地址与可选令牌。
#[derive(Debug, Clone)]
pub struct Config {
    pub url: String,
    pub token: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            url: std::env::var("CD2_URL")
                .ok()
                .filter(|url| !url.trim().is_empty())
                .unwrap_or_else(|| "http://127.0.0.1:19798".to_string()),
            token: std::env::var("CLOUDDRIVE_API_TOKEN")
                .ok()
                .or_else(|| std::env::var("CD2_TOKEN").ok())
                .filter(|token| !token.trim().is_empty()),
        }
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    url: String,
    token: Option<String>,
}

impl Client {
    pub fn new(config: &Config) -> Result<Self> {
        let url = normalize_url(&config.url);
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(10));
        // 本地 CD2 是明文 h2c，需要直接使用 HTTP/2 prior knowledge。
        if url.starts_with("http://") {
            builder = builder.http2_prior_knowledge();
        }
        let http = builder.build().context("创建 CD2 HTTP 客户端失败")?;
        Ok(Self {
            http,
            url,
            token: config.token.clone(),
        })
    }

    /// 读取完整的上传任务列表（包括已经完成/跳过的任务）。
    pub async fn upload_tasks(&self) -> Result<Vec<UploadTask>> {
        let endpoint = format!(
            "{}/clouddrive.CloudDriveFileSrv/GetUploadFileList",
            self.url
        );
        let mut request = self
            .http
            .post(&endpoint)
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .header("user-agent", concat!("embydown/", env!("CARGO_PKG_VERSION")))
            .body(grpc_envelope(&encode_upload_request()));
        if let Some(token) = &self.token {
            request = request.header("authorization", format!("Bearer {}", token.trim()));
        }

        let response = request
            .send()
            .await
            .with_context(|| format!("连接 CD2 gRPC 失败：{endpoint}"))?;
        let http_status = response.status();
        let grpc_status = response
            .headers()
            .get("grpc-status")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let grpc_message = response
            .headers()
            .get("grpc-message")
            .and_then(|value| value.to_str().ok())
            .map(percent_decode);
        let body = response
            .bytes()
            .await
            .context("读取 CD2 gRPC 响应失败")?;

        if !http_status.is_success() {
            bail!("CD2 返回 HTTP {http_status}");
        }
        if let Some(status) = &grpc_status
            && status != "0"
        {
            let message = grpc_message.unwrap_or_default();
            if status == "16" || message.to_ascii_lowercase().contains("unauthent") {
                bail!(
                    "CD2 拒绝访问（需要 API 令牌）：请在 CD2「设置 → API 令牌」创建令牌，\
                     然后设置 CLOUDDRIVE_API_TOKEN 或使用 --cd2-token"
                );
            }
            bail!("CD2 gRPC 错误 {status}：{message}");
        }
        if body.is_empty() {
            bail!(
                "CD2 返回空响应；如果 CD2 开启了 API 鉴权，请设置 CLOUDDRIVE_API_TOKEN 或 --cd2-token"
            );
        }

        let message = grpc_unwrap(&body)?;
        parse_upload_list(message)
    }
}

fn normalize_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("http://{url}")
    }
}

fn encode_upload_request() -> Vec<u8> {
    // GetUploadFileListRequest.getAll = true（字段 1，bool）
    vec![0x08, 0x01]
}

fn grpc_envelope(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 5);
    out.push(0); // 未压缩
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(message);
    out
}

fn grpc_unwrap(body: &[u8]) -> Result<&[u8]> {
    if body.len() < 5 {
        bail!("CD2 gRPC 响应格式无效（长度不足）");
    }
    let compressed = body[0];
    if compressed != 0 {
        bail!("CD2 gRPC 响应启用了压缩，当前版本不支持");
    }
    let length = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    let end = 5usize
        .checked_add(length)
        .context("CD2 gRPC 响应长度溢出")?;
    if end > body.len() {
        bail!(
            "CD2 gRPC 响应被截断：声明 {length} 字节，实际 {} 字节",
            body.len() - 5
        );
    }
    Ok(&body[5..end])
}

fn parse_upload_list(message: &[u8]) -> Result<Vec<UploadTask>> {
    let mut reader = Reader::new(message);
    let mut tasks = Vec::new();
    while !reader.eof() {
        let (field, wire) = reader.tag()?;
        if field == 2 && wire == 2 {
            let nested = reader.len_delimited()?;
            tasks.push(parse_upload_task(nested)?);
        } else {
            reader.skip(wire)?;
        }
    }
    Ok(tasks)
}

fn parse_upload_task(message: &[u8]) -> Result<UploadTask> {
    let mut reader = Reader::new(message);
    let mut task = UploadTask::default();
    while !reader.eof() {
        let (field, wire) = reader.tag()?;
        match (field, wire) {
            (1, 2) => task.key = text(reader.len_delimited()?),
            (2, 2) => task.dest_path = text(reader.len_delimited()?),
            (3, 0) => task.size = reader.varint()?,
            (4, 0) => task.transferred_bytes = reader.varint()?,
            (5, 2) => task.status = text(reader.len_delimited()?),
            (6, 2) => task.error = text(reader.len_delimited()?),
            (7, 0) => task.operator_type = reader.varint()? as i32,
            (8, 0) => task.status_enum = reader.varint()? as i32,
            _ => reader.skip(wire)?,
        }
    }
    Ok(task)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn parse_status_text(value: &str) -> i32 {
    match value.trim().to_ascii_lowercase().as_str() {
        "waitforpreprocessing" => 0,
        "preprocessing" => 1,
        "cancelled" | "canceled" => 2,
        "transfer" | "transfering" | "transferring" => 3,
        "pause" | "paused" => 4,
        "finish" | "finished" => 5,
        "skipped" | "skip" => 6,
        "inqueue" | "in_queue" => 7,
        "ignored" => 8,
        "error" => 9,
        "fatalerror" | "fatal_error" => 10,
        _ => {
            // 有些版本直接返回数字字符串。
            value.trim().parse::<i32>().unwrap_or(-1)
        }
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |byte: u8| -> Option<u8> {
                match byte {
                    b'0'..=b'9' => Some(byte - b'0'),
                    b'a'..=b'f' => Some(byte - b'a' + 10),
                    b'A'..=b'F' => Some(byte - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(high * 16 + low);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn eof(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let Some(&byte) = self.data.get(self.pos) else {
                bail!("protobuf 数据截断");
            };
            self.pos += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift >= 64 {
                bail!("protobuf varint 过长");
            }
        }
    }

    fn tag(&mut self) -> Result<(u32, u8)> {
        let tag = self.varint()?;
        let field = (tag >> 3) as u32;
        let wire = (tag & 0x07) as u8;
        if field == 0 {
            bail!("protobuf 字段编号为 0");
        }
        Ok((field, wire))
    }

    fn len_delimited(&mut self) -> Result<&'a [u8]> {
        let length = self.varint()? as usize;
        let end = self
            .pos
            .checked_add(length)
            .context("protobuf 长度溢出")?;
        if end > self.data.len() {
            bail!("protobuf 数据截断");
        }
        let value = &self.data[self.pos..end];
        self.pos = end;
        Ok(value)
    }

    fn skip(&mut self, wire: u8) -> Result<()> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => self.advance(8)?,
            2 => {
                self.len_delimited()?;
            }
            5 => self.advance(4)?,
            _ => bail!("不支持的 protobuf wire type：{wire}"),
        }
        Ok(())
    }

    fn advance(&mut self, count: usize) -> Result<()> {
        let end = self
            .pos
            .checked_add(count)
            .context("protobuf 长度溢出")?;
        if end > self.data.len() {
            bail!("protobuf 数据截断");
        }
        self.pos = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string_field(field: u8, value: &str) -> Vec<u8> {
        let mut out = vec![(field << 3) | 2];
        out.push(value.len() as u8);
        out.extend_from_slice(value.as_bytes());
        out
    }

    fn varint_field(field: u8, value: u64) -> Vec<u8> {
        let mut out = vec![field << 3];
        let mut value = value;
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
        out
    }

    #[test]
    fn parses_upload_list() {
        let mut task = Vec::new();
        task.extend(string_field(1, "key-1"));
        task.extend(string_field(2, "/影视/剧集/测试/S01E01.mkv"));
        task.extend(varint_field(3, 1024));
        task.extend(varint_field(4, 0));
        task.extend(string_field(5, "Skipped"));
        task.extend(varint_field(7, 0));
        task.extend(varint_field(8, 6));

        let mut list = vec![2 << 3 | 2, task.len() as u8];
        list.extend(task);
        let parsed = parse_upload_list(&list).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].dest_path, "/影视/剧集/测试/S01E01.mkv");
        assert_eq!(parsed[0].size, 1024);
        assert_eq!(parsed[0].status(), UploadStatus::Skipped);
    }

    #[test]
    fn recognizes_status_names() {
        assert_eq!(parse_status_text("Finish"), 5);
        assert_eq!(parse_status_text("skipped"), 6);
        assert_eq!(parse_status_text("Transfer"), 3);
        assert_eq!(UploadStatus::from_code(6), UploadStatus::Skipped);
    }

    #[test]
    fn unwraps_grpc_envelope() {
        let message = b"abc";
        let mut body = vec![0, 0, 0, 0, 3];
        body.extend_from_slice(message);
        assert_eq!(grpc_unwrap(&body).unwrap(), message);
        assert!(grpc_unwrap(&[0, 0]).is_err());
    }
}
