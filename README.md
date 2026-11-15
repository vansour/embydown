# embydown

[![CI](https://github.com/vansour/embydown/actions/workflows/ci.yml/badge.svg)](https://github.com/vansour/embydown/actions/workflows/ci.yml)
[![Release](https://github.com/vansour/embydown/actions/workflows/release.yml/badge.svg)](https://github.com/vansour/embydown/actions/workflows/release.yml)

用 Rust 写的 Emby 媒体下载工具。默认走直链（`/Videos/{id}/stream?static=true`，失败回退 `/Items/{id}/Download`），支持分块并发、断点续传、整季批量下载与外挂字幕。**仅适配 Linux。**

不带子命令直接运行 `embydown` 会进入命令行交互模式；`embydown ui` 等价。

## 安装

```bash
curl -fsSL https://raw.githubusercontent.com/vansour/embydown/main/install.sh | bash

# 指定版本 / 安装目录
curl -fsSL https://raw.githubusercontent.com/vansour/embydown/main/install.sh | bash -s -- --version v0.0.1
curl -fsSL https://raw.githubusercontent.com/vansour/embydown/main/install.sh | bash -s -- --prefix "$HOME/.local"

# 卸载
curl -fsSL https://raw.githubusercontent.com/vansour/embydown/main/install.sh | bash -s -- --uninstall
```

默认安装到 `/usr/local/bin`（root）或 `~/.local/bin`（普通用户），安装前校验 SHA-256。
预编译包为 x86_64 + glibc；其它平台从源码构建（Rust 1.99+，`cargo build --release`）。

安装后直接用 `embydown update` 升级：

```bash
embydown update                 # 最新正式版
embydown update --check         # 只检查版本
embydown update --version v0.0.10
embydown update --force         # 已是最新也重新安装
```

安装目录需要写权限（如 `/usr/local/bin` 用 `sudo embydown update`）。

## 登录

凭证保存在 `~/.config/embydown/config.json`（0600），可同时登录多个服务器。

```bash
embydown login --server https://media.example.com --user 用户名 --password 密码
embydown login --server https://other.example.com --user 用户名 --password 密码 --name other
embydown login --server http://192.168.1.10:8096 --api-key <API_KEY>
embydown login --server https://example.com --user 用户名 --password 密码 --insecure   # 自签证书

embydown servers
embydown logout [--name other]
```

## 交互模式

```bash
embydown        # 或 embydown ui
```

| 菜单 | 功能 |
|---|---|
| 1 搜索并下载 | 跨服务器搜索：先选影片、再选来源；也接受 `http(s)://` 直链 |
| 2 浏览媒体库 | 服务器 → 媒体库 → 剧集 / 季 / 分集逐层浏览；`d` 下载当前目录 |
| 3 下载队列 | 实时进度；暂停/继续、取消任务、重试失败、清空记录 |
| 4 服务器管理 | 添加登录、删除、设置搜索范围 |
| 5 下载设置 | 下载文件夹、连接数、并发、限速、字幕、覆盖、续传、命名模板 |
| 6 未完成任务找回 | 扫描 `.part` / `.part.json`，搜索来源并写回原路径续传 |
| 7 下载记录 | 查看历史记录 |
| 8 本地审查 | 检查下载文件夹：缺集、重复集、未完成与异常文件 |

回车取消或返回上一级，`0` 返回/退出，`Ctrl+C` 随时退出。设置修改后立即保存。
下载队列按 Enter 操作：`p`/`P` 暂停，`c` 取消任务，`x`/`X` 取消队列，`R` 重试，
`e` 查看失败原因，`C` 清空记录，`u` 找回未完成任务。

下载在独立后台服务中执行，退出交互模式或关闭终端不会中断。

## 命令行

```bash
# 列出媒体库 / 条目
embydown ls [--server NAME]
embydown ls --parent <ID> --recursive --kinds Episode

# 搜索预览（不下载）
embydown dl 关键字 --dry-run

# 下载：关键字、ID、名称:ID、整剧/整季 ID 或 http(s) 直链
embydown dl 关键字
embydown dl <ID> -o ~/Downloads
embydown dl <SERIES_ID> --season 1 --season 2
embydown dl <ID> --version 2160p    # 名称子串、序号或 all

# 条目详情 / 只打印直链
embydown info <ID>
embydown url <ID>

# 后台队列
embydown pause [--server NAME]
embydown resume [--server NAME]
embydown history [--limit 0] [--clear]

# 本地审查 / 自更新
embydown review [--dir ./downloads]
embydown update
```

常用参数：

| 参数 | 说明 |
|---|---|
| `-o, --out <DIR>` | 下载文件夹，默认配置文件中的值（`./downloads`） |
| `-c, --connections <N>` | 每文件并发连接数（默认 2） |
| `-j, --jobs <N>` | 同时下载文件数（默认 2） |
| `--limit-rate 5M` | 限速 |
| `--version <VALUE>` | 多版本时选序号、名称子串或 `all` |
| `--season <N>` | 只下载指定季，可重复 |
| `--dry-run` | 只列出匹配结果与文件信息 |
| `--pattern <TEMPLATE>` | 自定义命名模板 |
| `--no-subs` / `--no-resume` / `--no-overwrite` / `--no-limit-rate` | 反向覆盖配置 |

## 下载文件夹与命名

下载直接保存到**下载文件夹**，默认结构：

- 电影：`影视/电影/{title} ({year}) [tmdbid-…]/{title} ({year}) [tmdbid-…].{ext}`
- 剧集：`影视/剧集/{series} ({seriesyear}) [tmdbid-…]/Season {season00}/{series} - S{season00}E{episode00}.{ext}`
- 音乐 / 其他：`影视/音乐/{title}.{ext}`、`影视/其他/{title}.{ext}`

`[tmdbid-…]` 是 Emby/Jellyfin 支持的元数据 ID 标记，服务器返回 TMDB ID 时自动追加，
取不到时整段省略。剧集使用**所属剧集**的 TMDB ID（单集自身的 TMDB ID 不用于目录命名），
直接下载单集/单季时会自动查询一次剧集详情。

`--pattern` 占位符：

```
{title} {series} {season} {season00} {season5} {episode} {episode00} {episode5} {year} {seriesyear} {tmdbid} {tmdbidtag} {version} {ext} {type} {id} {container}
```

`{season00}` / `{episode00}` 至少两位补零（`01`、`99`），季/集超过 99 时自动使用三位（`100`）；
`{season5}` / `{episode5}` 固定五位。`{tmdbid}` 输出原始 ID；`{tmdbidtag}` 输出
` [tmdbid-123]`（含前导空格，缺失时为空），可写成 `{title} ({year}){tmdbidtag}`。

`--version all` 下载多个版本时，版本名会加入上一级目录避免覆盖；路径中的非法字符替换为 `_`。

## 本地审查

`embydown review` 在本地检查下载文件夹，不访问服务器、不打开文件内容：

- 统计视频 / 字幕 / 元数据 / 图片与总大小，列出剧集和影片
- 按 `SxxExx` 报告缺集与重复集
- 检查空文件、过小视频和 `.part` / `.part.json` 等未完成文件

`--dir` 指定目录，`-v` 展开完整列表。

## 断点续传

- 分块模式把进度写入 `.part.json`，重跑同一条命令即可续传；`-c 1` 从 `.part` 续写
- 目标文件已存在且大小一致时自动跳过；大小不一致时默认报错，确认替换加 `--overwrite`
- 想重新下载：加 `--overwrite` 或删除 `.part` / `.part.json`
- `.part` 是稀疏文件：`ls -l` 显示完整大小，`du` 才是实际占用

## 后台服务

交互模式的下载由独立的 `embydown daemon` 执行，会自动拉起。状态都在 `~/.config/embydown/`：

- `queue.json`：排队/运行中的任务，重启后配合 `.part` 续传
- `history.jsonl`：下载记录；`daemon.log`：后台日志
- 单实例通信：`$XDG_RUNTIME_DIR/embydown/daemon.sock`

前台观察可运行 `embydown daemon`。

## 排错

| 现象 | 处理 |
|---|---|
| `HTTP 401` | Token / API Key 失效，重新登录 |
| `HTTP 403` | 账号没有该媒体的播放或下载权限 |
| `HTTP 404` | 条目 ID 不对，或直链路径不被服务器支持 |
| `HTTP 416` | 服务端文件被替换，删除 `.part` / `.part.json` 重下 |
| `HTTP 429` / Cloudflare `1015` | 触发限流；短时限流会自动等待，稍后重跑可续传 |
| `HTTP 404 page not found` | 地址漏了 `https://` |
| 卡在探测阶段 | 反向代理未透传 `Range`，用 `-c 1` 单连接 |
| 后台服务连接失败 | 查看 `~/.config/embydown/daemon.log`，或前台运行 `embydown daemon` |
| 自签证书报错 | 加 `--insecure` |

调试：`-v` 常规日志，`-vv` 直链探测详情（`api_key` 自动打码）。

## 开发

```bash
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
```

版本变更见 [CHANGELOG.md](CHANGELOG.md)；推送 `v*` 标签触发 Release 构建。
