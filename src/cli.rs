//! 命令行定义。

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "embydown",
    version,
    about = "Emby 媒体下载工具（默认直链：static stream / Download）",
    long_about = "Emby 媒体下载工具。\n\n直接运行 `embydown`（或 `embydown ui`）进入命令行交互模式：数字菜单选择搜索、浏览媒体库、管理服务器、下载队列与设置。\n也可以使用子命令：先运行 `embydown login --server <URL> --user <账号> --password <密码>` 保存登录信息；\n可重复 login（配合 --name）同时登录多个服务器，`embydown servers` 查看。\ndl 默认覆盖所有服务器；`dl <关键字> --dry-run` 会列出匹配文件的大小、分辨率、编码等信息。\n默认使用直链下载原始文件（/Videos/{id}/stream?static=true，失败回退 /Items/{id}/Download），\n支持分块并发、断点续传与整季批量下载。\n下载先落到临时文件夹；`embydown review` 在本地审查缺集/异常，`embydown sync` 复制到最终文件夹。\n最终文件夹是 CD2 等网盘挂载时，可用 `embydown upload-test <剧名>` 只下载第一集并检测是否秒传。\n安装一次后可运行 `embydown update` 自更新，无需 curl。"
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
    /// 进入命令行交互模式（直接运行 embydown 也会进入）
    #[command(alias = "interactive")]
    Ui,

    /// 运行后台下载服务（交互模式会自动启动；一般无需手动执行）
    Daemon,

    /// 检查并安装新版本（从 GitHub Releases 下载并校验 SHA-256）
    Update {
        /// 只检查是否有新版本，不下载安装
        #[arg(long)]
        check: bool,

        /// 安装指定版本标签（如 v0.0.5 或 0.0.5），默认最新正式版
        #[arg(long, value_name = "TAG")]
        version: Option<String>,

        /// 即使已经是最新版本也重新下载安装
        #[arg(long)]
        force: bool,
    },

    /// 查看永久保存的下载记录（最新在前）
    #[command(alias = "records")]
    History {
        /// 显示条数；0 表示全部
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },

    /// 本地审查临时/最终文件夹：内容、缺集、重复集、未完成与损坏文件、同步差异
    #[command(alias = "check", alias = "inspect")]
    Review {
        /// 临时文件夹（默认使用配置中的下载目录）
        #[arg(long, value_name = "DIR")]
        temp: Option<PathBuf>,

        /// 最终文件夹（默认使用配置中的最终文件夹）
        #[arg(long = "final", value_name = "DIR")]
        final_dir: Option<PathBuf>,
    },

    /// 将临时文件夹的内容同步（复制）到最终文件夹
    #[command(alias = "copy")]
    Sync {
        /// 只显示将要复制的内容，不实际写入
        #[arg(long)]
        dry_run: bool,

        /// 即使大小相同也重新复制
        #[arg(long)]
        force: bool,

        /// 临时文件夹（默认使用配置中的下载目录）
        #[arg(long, value_name = "DIR")]
        temp: Option<PathBuf>,

        /// 最终文件夹（默认使用配置中的最终文件夹）
        #[arg(long = "final", value_name = "DIR")]
        final_dir: Option<PathBuf>,
    },

    /// 秒传测试：只下载第一集并移动到最终文件夹，查询 CD2 上传任务判断是否秒传
    #[command(alias = "quicktest", alias = "instant-test")]
    UploadTest {
        #[command(flatten)]
        server: ServerArg,

        /// 条目 ID、片名/剧名关键字，或 `服务器:ID`
        target: String,

        /// 按关键字匹配时的结果上限
        #[arg(long, default_value_t = 30)]
        limit: u32,

        /// CD2 地址：原生 gRPC（http://host:19798）或 CD2 Web UI/API 地址（自动使用 gRPC-Web）
        #[arg(long, value_name = "URL")]
        cd2_url: Option<String>,

        /// CD2 API 令牌（默认使用配置中的令牌或 CLOUDDRIVE_API_TOKEN）
        #[arg(long, value_name = "TOKEN")]
        cd2_token: Option<String>,

        /// 观察 CD2 上传任务的秒数
        #[arg(long, default_value_t = 20)]
        window: u64,

        /// 保留最终文件夹中的测试目录（默认测试后删除）
        #[arg(long)]
        keep: bool,

        /// 临时文件夹（默认使用配置中的下载目录）
        #[arg(long, value_name = "DIR")]
        temp: Option<PathBuf>,

        /// 最终文件夹（默认使用配置中的最终文件夹）
        #[arg(long = "final", value_name = "DIR")]
        final_dir: Option<PathBuf>,
    },

    /// 暂停后台下载队列（先停止派发，正在下载的任务放回队首，保留断点）
    Pause {
        /// 只暂停指定服务器（login --name 的别名）；省略则暂停全部
        #[arg(long, value_name = "NAME")]
        server: Option<String>,
    },

    /// 继续被暂停的后台下载队列
    Resume {
        /// 只继续指定服务器；省略则继续全部
        #[arg(long, value_name = "NAME")]
        server: Option<String>,
    },

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

    /// 只打印直链（static stream / Download），可粘到浏览器/IDM
    #[command(alias = "link")]
    Url {
        #[command(flatten)]
        server: ServerArg,

        id: String,
    },

    /// 下载条目（电影 / 单集 / 整季 / 整剧 / 媒体库）
    #[command(alias = "dl")]
    Download {
        #[command(flatten)]
        server: ServerArg,

        /// 条目 ID（可写 `名称:ID`）、片名关键字，或 http(s) 直链
        target: String,

        /// 临时文件夹（下载目录；省略时使用配置文件中的值，默认 ./downloads）
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,

        /// 命名模板，如 "{series} ({seriesyear})/Season {season5}/{series} - S{season5}E{episode5}.{ext}"
        #[arg(long)]
        pattern: Option<String>,

        /// 每个文件的并发连接数（分块）；省略时用配置文件中的值
        #[arg(short = 'c', long)]
        connections: Option<usize>,

        /// 同时下载几个文件；省略时用配置文件中的值
        #[arg(short = 'j', long)]
        jobs: Option<usize>,

        /// 限速，如 5M / 800K；省略时用配置文件中的值
        #[arg(long, conflicts_with = "no_limit_rate")]
        limit_rate: Option<String>,

        /// 本次不限速（覆盖配置文件）
        #[arg(long, conflicts_with = "limit_rate")]
        no_limit_rate: bool,

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

        /// 覆盖已存在文件（覆盖配置文件）
        #[arg(long, conflicts_with = "no_overwrite")]
        overwrite: bool,

        /// 不覆盖已存在文件（覆盖配置文件）
        #[arg(long, conflicts_with = "overwrite")]
        no_overwrite: bool,

        /// 下载外挂字幕（覆盖配置文件）
        #[arg(long, conflicts_with = "no_subs")]
        subs: bool,

        /// 不下载外挂字幕（覆盖配置文件）
        #[arg(long, conflicts_with = "subs")]
        no_subs: bool,

        /// 启用断点续传（覆盖配置文件）
        #[arg(long, conflicts_with = "no_resume")]
        resume: bool,

        /// 关闭断点续传（覆盖配置文件）
        #[arg(long, conflicts_with = "resume")]
        no_resume: bool,

        /// 只列出匹配结果与文件信息（大小、分辨率、编码），不下载
        #[arg(long)]
        dry_run: bool,
    },
}
