//! 命令行定义。

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "embydown",
    version,
    about = "Emby 媒体下载工具（默认直链：static stream / Download）",
    long_about = "Emby 媒体下载工具。\n\n直接运行 `embydown`（或 `embydown ui`）进入交互界面：搜索、浏览媒体库、管理服务器、选择版本/季并下载。\n也可以使用子命令：先运行 `embydown login --server <URL> --user <账号> --password <密码>` 保存登录信息；\n可重复 login（配合 --name）同时登录多个服务器，`embydown servers` 查看。\ndl 默认覆盖所有服务器；`dl <关键字> --dry-run` 会列出匹配文件的大小、分辨率、编码等信息。\n默认使用直链下载原始文件（/Videos/{id}/stream?static=true，失败回退 /Items/{id}/Download），\n支持分块并发、断点续传与整季批量下载。"
)]
pub struct Cli {
    /// 详细日志：-v 常规，-vv 调试
    #[arg(short = 'v', long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

/// `login` 子命令参数。
#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Emby 服务器地址，如 https://media.example.com
    #[arg(long)]
    pub server: String,

    /// 服务器别名（省略时用主机名），用于同时登录多个服务器
    #[arg(long)]
    pub name: Option<String>,

    /// 用户名（与 --password 搭配）
    #[arg(long)]
    pub user: Option<String>,

    /// 密码
    #[arg(long)]
    pub password: Option<String>,

    /// API Key（与用户名密码二选一）
    #[arg(long)]
    pub api_key: Option<String>,

    /// 跳过 TLS 证书校验（自签证书服务器）
    #[arg(long)]
    pub insecure: bool,
}

/// 指定已登录的服务器（按 `login --name` 的别名）。
#[derive(Debug, Args, Clone, Default)]
pub struct ServerArg {
    /// 服务器名称；省略时：dl（含 --dry-run）查询所有服务器，其他命令在只有一个服务器时自动使用
    #[arg(long, value_name = "NAME")]
    pub server: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// 打开交互界面（直接运行 embydown 也会进入）
    #[command(alias = "tui")]
    Ui,

    /// 登录并保存服务器与凭证（可重复执行以添加多个服务器）
    Login(LoginArgs),

    /// 退出登录：省略 --name 时清除所有服务器
    Logout {
        /// 只退出指定名称的服务器
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },

    /// 列出已登录的服务器
    #[command(alias = "profiles")]
    Servers,

    /// 列出媒体库 / 某个文件夹下的条目
    #[command(alias = "ls")]
    List {
        #[command(flatten)]
        server: ServerArg,

        /// 父条目 ID；省略则列出媒体库
        #[arg(long)]
        parent: Option<String>,

        /// 递归展开子项
        #[arg(long)]
        recursive: bool,

        /// 只看这些类型，逗号分隔：Movie,Episode,Series,Audio
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,

        /// 结果上限
        #[arg(long, default_value_t = 200)]
        limit: u32,
    },

    /// 显示单个条目的详情
    #[command(alias = "show")]
    Info {
        #[command(flatten)]
        server: ServerArg,

        id: String,
    },

    /// 只打印直链（默认直链，加 --transcode 出转码链），可粘到浏览器/IDM
    #[command(alias = "link")]
    Url {
        #[command(flatten)]
        server: ServerArg,

        id: String,
        #[command(flatten)]
        transcode: TranscodeArgs,
    },

    /// 下载条目（电影 / 单集 / 整季 / 整剧 / 媒体库）
    #[command(alias = "dl")]
    Download {
        #[command(flatten)]
        server: ServerArg,

        /// 条目 ID（可写 `名称:ID`）、片名关键字，或 http(s) 直链
        target: String,

        /// 输出目录
        #[arg(short = 'o', long, default_value = "./downloads")]
        out: PathBuf,

        /// 命名模板，如 "{series} ({seriesyear})/Season {season5}/{series} - S{season5}E{episode5}.{ext}"
        #[arg(long)]
        pattern: Option<String>,

        /// 每个文件的并发连接数（分块）
        #[arg(short = 'c', long, default_value_t = 8)]
        connections: usize,

        /// 同时下载几个文件
        #[arg(short = 'j', long, default_value_t = 2)]
        jobs: usize,

        /// 限速，如 5M / 800K
        #[arg(long)]
        limit_rate: Option<String>,

        /// 按关键字匹配时的结果上限
        #[arg(long, default_value_t = 30)]
        limit: u32,

        /// 多版本时选择：序号（从 1 开始，按版本名排序）、名称子串（如 2160p），或 all
        /// 不指定时会列出所有版本让你选；指定后跳过不包含该版本的集
        #[arg(long, value_name = "VALUE")]
        version: Option<String>,

        /// 只下载指定季（仅剧集有效，可重复）
        #[arg(long)]
        season: Vec<i32>,

        /// 覆盖已存在文件
        #[arg(long)]
        overwrite: bool,

        /// 不下载外挂字幕
        #[arg(long)]
        no_subs: bool,

        /// 关闭断点续传
        #[arg(long)]
        no_resume: bool,

        /// 只列出匹配结果与文件信息（大小、分辨率、编码），不下载
        #[arg(long)]
        dry_run: bool,

        #[command(flatten)]
        transcode: TranscodeArgs,
    },
}

#[derive(Debug, Args, Clone, Default)]
pub struct TranscodeArgs {
    /// 不用直链，改用服务端转码流
    #[arg(long)]
    pub transcode: bool,

    /// 转码容器：mp4 / mkv / ts
    #[arg(long, default_value = "mp4")]
    pub container: String,

    /// 视频编码：h264 / hevc
    #[arg(long)]
    pub video_codec: Option<String>,

    /// 音频编码：aac / mp3
    #[arg(long)]
    pub audio_codec: Option<String>,

    /// 最大高度：1080 / 720
    #[arg(long)]
    pub max_height: Option<u32>,

    /// 视频码率（kbps）
    #[arg(long)]
    pub video_bitrate: Option<u32>,

    /// 音频码率（kbps）
    #[arg(long)]
    pub audio_bitrate: Option<u32>,
}

impl TranscodeArgs {
    pub fn to_opts(&self) -> Option<crate::model::TranscodeOpts> {
        if !self.transcode {
            // 显式给了转码参数也视为要转码
            let touched = self.video_codec.is_some()
                || self.audio_codec.is_some()
                || self.max_height.is_some()
                || self.video_bitrate.is_some()
                || self.audio_bitrate.is_some()
                || self.container != "mp4";
            if !touched {
                return None;
            }
        }
        Some(crate::model::TranscodeOpts {
            container: self.container.clone(),
            video_codec: self.video_codec.clone(),
            audio_codec: self.audio_codec.clone(),
            max_height: self.max_height,
            video_bitrate: self.video_bitrate,
            audio_bitrate: self.audio_bitrate,
        })
    }
}
