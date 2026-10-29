# embydown

[![CI](https://github.com/vansour/embydown/actions/workflows/ci.yml/badge.svg)](https://github.com/vansour/embydown/actions/workflows/ci.yml)
[![Release](https://github.com/vansour/embydown/actions/workflows/release.yml/badge.svg)](https://github.com/vansour/embydown/actions/workflows/release.yml)

用 Rust 写的 Emby 媒体下载工具。**默认走直链**（`/Videos/{id}/stream?static=true`，失败自动回退官方 `/Items/{id}/Download`），支持分块并发、断点续传、整季批量、外挂字幕。

不带任何子命令直接运行 `embydown` 会进入**交互界面（TUI）**；`embydown ui` 等价。

> **仅适配 Linux**（ext4 / xfs / btrfs / tmpfs 实测）。Windows 的文件名与 rename 语义不在支持范围内。

## 安装

```bash
# 一键安装最新版
curl -fsSL https://raw.githubusercontent.com/vansour/embydown/main/install.sh | bash

# 指定版本 / 安装目录
curl -fsSL https://raw.githubusercontent.com/vansour/embydown/main/install.sh | bash -s -- --version v0.0.1
curl -fsSL https://raw.githubusercontent.com/vansour/embydown/main/install.sh | bash -s -- --prefix "$HOME/.local"

# 卸载
curl -fsSL https://raw.githubusercontent.com/vansour/embydown/main/install.sh | bash -s -- --uninstall
```

- 默认安装到 `/usr/local/bin`（root）或 `~/.local/bin`（普通用户）
- 安装前会校验 Release 附带的 SHA-256
- 目前提供 **x86_64 + glibc** 预编译包；ARM / Alpine(musl) 请从源码构建
- 也可以直接在 [Releases](https://github.com/vansour/embydown/releases) 下载
  `embydown-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz`

## 构建

需要 Rust **1.99+**（edition 2024）。

```bash
cargo build --release
# 产物：target/release/embydown
```

## 认证

凭证保存在 `~/.config/embydown/config.json`（权限 0600），之后所有子命令直接使用。
可以同时登录多个服务器；默认用主机名作为别名，也可用 `--name` 自定义。

```bash
# 账号密码（走 /Users/AuthenticateByName 换 Token）
embydown login --server https://media.micu.hk --user 用户名 --password 密码

# 再登录第二个服务器（不会覆盖第一个）
embydown login --server https://v1-vod2.uhdnow.com --user 用户名 --password 密码 --name vod2

# API Key
embydown login --server http://192.168.1.10:8096 --api-key <API_KEY>

# 自签证书服务器
embydown login --server https://example.com --user 用户名 --password 密码 --insecure

# 查看已登录的服务器
embydown servers

# 退出单个 / 全部
embydown logout --name vod2
embydown logout
```

## 交互界面（TUI）

```bash
embydown          # 直接进入
embydown ui       # 等价写法
```

五个页面（`Tab` / `1-5` 切换）：

| 页面 | 功能 |
|---|---|
| 搜索 | 跨服务器搜索；也接受 `http(s)://` 直链，`Enter` 打开下载确认 |
| 浏览 | 服务器 → 媒体库 → 剧集 / 季 / 分集逐层浏览 |
| 下载 | 后台下载队列：每个服务器一条独立下载流，显示进度、排队、完成/失败统计 |
| 服务器 | `a` 登录（账号密码 / API Key）、`d` 删除、`Enter` 设为搜索范围 |
| 设置 | 输出目录、连接数、每服务器并发、限速、字幕、覆盖、续传、命名模板（修改后立即保存） |

「设置」页的修改会立即写入配置文件（与登录信息同一个 `config.json`），重启 TUI 后仍然生效；
`logout` 只清除登录信息，不会重置这些下载设置。

命令行 `embydown dl` 也把这些设置作为默认值（输出目录、连接数、并发、限速、字幕、覆盖、续传、命名模板），
命令行显式传参时以命令行为准。需要临时反着覆盖时可用 `--no-overwrite`、`--no-resume`、`--no-subs`、
`--no-limit-rate`；`--pattern ""` 表示本次使用内置默认命名模板。

下载确认页会先解析条目：

- 列出所有版本及各自实际集数、体积、分辨率、容器；**必须用数字选择版本**（`0` / `a` = 全部版本）
- `f` 选择要下载的季，`u` 查看直链，`Enter` 开始下载，`Esc` 取消
- 搜索页和下载详情页的版本归类与 CLI 的 `--dry-run` / `--version` 完全一致

开始下载后界面不会退出：任务进入后台队列，可以继续搜索、浏览或切换页面。
每个服务器有独立的队列、并发数和限速器，多个服务器会同时下载、互不阻塞；
同一服务器内的任务按加入顺序排队，并发数由「每服务器并发数」控制。

下载页按键：`↑↓` 选择服务器，`x` 取消该服务器的队列，`X` 取消全部，`c` 清空最近完成记录。
取消或退出时已完成的分块会保留，重新加入同一任务即可续传。

按键：`1-5` / `Tab` 切页，`↑↓` 选择，`Enter` 确认，`Esc` 返回，`?` 查看全部快捷键，`Ctrl+C` 随时退出。
搜索输入框内普通按键会输入文字，因此切页请用 `Tab`。

## 常用命令

```bash
# 列出媒体库（只有一个服务器时可直接用；多个服务器用 --server 指定）
embydown ls
embydown ls --server media.micu.hk

# 列出某个库 / 剧集下的条目（--recursive 展开，--kinds 过滤类型）
embydown ls --server media.micu.hk --parent <PARENT_ID> --recursive --kinds Episode

# 搜索 / 预览：以表格列出所有服务器上匹配的文件（大小、分辨率、视频/音频编码、容器），不下载
# 剧集按季分行：同一集有多个媒体源时再按版本拆行（如 ANi / Nekomoe / 1080p - BluRay）；
# 每集只有一个源的季不拆分，VERSION 显示统一片源或「多种片源」
embydown dl 关键字 --dry-run
embydown dl 关键字 --dry-run --server media.micu.hk
embydown dl 关键字 --dry-run --limit 50

# 去掉 --dry-run 即可交互选择并下载
embydown dl 关键字 -o ./downloads
# 输出目录支持 ~ 展开：-o ~/Downloads 会存到 $HOME/Downloads

# 查看条目详情（含媒体源、流信息、大小）
embydown info <ITEM_ID> --server media.micu.hk

# 只打印直链（可粘到浏览器 / IDM；带 api_key，注意别外传）
embydown url <ITEM_ID> --server media.micu.hk

# 按 ID / 名称:ID 精确下载（list 等命令可看到 ID）
embydown dl <ITEM_ID> -o ./downloads
embydown dl media.micu.hk:<ITEM_ID> -o ./downloads

# 整剧 / 整季（Series / Season / 媒体库 ID 都能直接丢进去）
embydown dl <SERIES_ID> -o ./downloads --season 1 --season 2

# 多版本（同一部片/同一集有 1080p / 2160p 或多个发布组）：
#   --dry-run 会为每个版本单独列一行，并显示该版本实际包含的集数
#   下载时不指定 --version 会列出所有版本让你选（非交互环境必须用 --version）
#   指定后只下载包含所选版本的集，不匹配的集会自动跳过
#   序号从 1 开始，按版本名排序；也可用名称子串或 all（显式下载全部版本）
embydown dl <ITEM_ID> --dry-run
embydown dl <ITEM_ID>
embydown dl <ITEM_ID> --version 2160p
embydown dl <ITEM_ID> --version 2
embydown dl <ITEM_ID> --version all

# 限速 5MB/s、每文件 8 连接、同时 2 个文件
embydown dl <ITEM_ID> --limit-rate 5M -c 8 -j 2

# 直接给一个 http(s) 直链也能下（复用同一套分块/续传）
embydown dl "http://host:8096/Videos/<ID>/stream?static=true&api_key=<KEY>" -o ./downloads
```

## 已在真实服务器验证

| 服务器 | 特性 | 结果 |
|---|---|---|
| `media.micu.hk` | Emby 4.9.x，直链直接 206（`video/x-matroska`） | 直链下载 8.86 MiB 完整通过，字节数与 `MediaSources.Size` 精确一致、`ftyp` 头正确 |
| `v1-vod2.uhdnow.com` | Emby 4.9.3，静态直链 **302 → `/play/video/{id}` CDN** | 自动跟随重定向、Range 保留；并发 4 分块 0 失败；单连接断点续传 1.44 MiB → 2.64 MB 通过 |

真实环境还确认了两点：

- 服务器若限制「允许下载」，`/Items/{id}/Download` 会 403，静态直链仍可用 → 工具默认优先静态直链正是为此。
- 用 `http://` 打头访问只支持 https 的站，会被 301 重定向，登录 POST 被降级成 GET → 报 `HTTP 404 page not found`。**地址请写全 `https://`**。

## 直链是怎么选的

按顺序探测，第一个回 `200/206` 的就用：

| 顺序 | URL | 说明 |
|---|---|---|
| 1 | `/Videos/{id}/stream?static=true` | 静态直链，只需播放权限；音频走 `/Audio/{id}/stream?static=true` |
| 2 | `/Videos/{id}/stream.{ext}?static=true` | 部分老版本只认带容器后缀的路径 |
| 3 | `/Items/{id}/Download` | 官方下载接口，需要账号有「允许下载」权限 |
| 4 | `/Items/{id}/File` | 兜底，部分版本/管理员可用 |

探测用 `Range: bytes=0-0`：返回 `206` 说明支持分段，于是按 `--connections` 并发拉块；返回 `200` 说明不支持，自动退化成单连接流式下载。

## 断点续传

- 分块模式：每完成一块就写入 `<文件名>.part.json`（记录总大小、块大小、已完成块号）。中断后**重跑同一条命令**即可续传。
- 单连接模式：`-c 1` 时从 `.part` 已有长度用 `Range: bytes=N-` 续写。
- 想重新下：加 `--overwrite` 或删掉 `.part` / `.part.json`。
- 关闭续传：`--no-resume`。
- `.part` 用 `set_len` 预分配，在 Linux 上是**稀疏文件**：`ls -l` 看到的是完整大小，`du` 才是真实占用（实测 8.47 GB 的片只占 6.8 MB）。

## 命名模板

`--pattern` 支持占位符：

```
{title} {series} {season} {season00} {season5} {episode} {episode00} {episode5} {year} {seriesyear} {version} {ext} {type} {id} {container}
```

`{season5}` / `{episode5}` 是 5 位补零编号（S00001E00001），`{year}` 是条目自身年份，
`{seriesyear}` 是剧集年份（展开时从 Series 容器或服务器路径补全），
`{version}` 是多版本时的版本名（如 `2160p - WEB-DL`）。

默认（Emby 标准目录结构，输出目录下自动分影视/电影、影视/剧集）：

- 电影：`影视/电影/{title} ({year})/{title} ({year}).{ext}`
- 剧集：`影视/剧集/{series} ({seriesyear})/Season {season5}/{series} - S{season5}E{episode5}.{ext}`
- 音乐 / 其他：`影视/音乐/{title}.{ext}`、`影视/其他/{title}.{ext}`

剧集文件名只保留剧名和 `SxxxxxExxxxx` 编号，不写单集标题。
只在一次下载多个版本（`--version all`）时把版本名放进上一级目录，避免互相覆盖；平时选定一个版本下载不生成版本目录：

- 电影：`影视/电影/片名 (2026) [2160p - WEB-DL HDR]/片名 (2026).mkv`
- 剧集：`影视/剧集/剧名 (2023) [1080p]/Season 00001/剧名 - S00001E00001.mkv`

示例：

```bash
embydown dl <ID> --pattern '影视/剧集/{series} ({seriesyear})/Season {season5}/{series} - S{season5}E{episode5} - {title}.{ext}'
```

路径里的非法字符（`<>:"/\|?*` 与控制字符）会自动替换成 `_`，单段超长会截断。

## 字幕

默认下载外挂字幕（`--no-subs` 关闭）：从 `MediaStreams` 里取 `Type=Subtitle && IsExternal=true` 的流，优先用服务器给的 `DeliveryUrl`，否则按 `/Videos/{id}/{msId}/Subtitles/{index}/Stream.{codec}` 构造。文件名与视频同名并追加语言，例如 `xxx.zh.srt`。

## 发布

版本号在 `Cargo.toml`；推送 `v*` 标签会触发 `.github/workflows/release.yml`：

- 校验标签与 `Cargo.toml` 版本一致（例如标签 `v0.0.1` 对应版本 `0.0.1`）
- 构建 x86_64 release，打包二进制、README 和 install.sh，生成 SHA-256
- 创建 GitHub Release 并上传产物

```bash
git tag v0.0.1
git push origin v0.0.1
```

`main` 分支的 push / PR 会运行 `.github/workflows/ci.yml`（clippy、测试、release 构建）。

## 排错

| 现象 | 原因 / 处理 |
|---|---|
| `HTTP 401` | Token 失效；或 API Key 被删。换 Key / 重新登录 |
| `HTTP 403` | 账号没有该媒体的播放/下载权限；用管理员账号或在 Emby 里给该用户开权限 |
| `HTTP 404` | 条目 ID 不对，或直链路径不被该版本支持（工具会自动试下一个候选） |
| `HTTP 416` | 文件被服务端替换，删掉 `.part` 与 `.part.json` 重下 |
| `HTTP 404 page not found` | 地址漏了 `https://`，被 301 后登录 POST 降级成 GET。补全 scheme |
| 直链 302 | 部分服务器把直链跳到 `/play/video/{id}` CDN；reqwest 自动跟随且保留 `Range`，无需处理 |
| 卡在探测阶段 | 服务器前面有反代且未透传 `Range` 头；用 `-c 1` 走单连接 |
| 速度上不去 | 服务器出口/磁盘瓶颈；`-c` 调到 4–16 之间试；`-j` 控制文件级并发 |
| 自签证书报错 | 加 `--insecure` |

调试：`-v` 看常规日志，`-vv` 看每个候选直链的探测结果（`api_key` 会自动打码）。

## 目录结构

```
src/main.rs      CLI 分发、条目展开、表格输出
src/cli.rs       clap 定义
src/config.rs    登录配置（~/.config/embydown/config.json）
src/emby.rs      Emby API 客户端（认证 / 浏览 / 直链构造）
src/download.rs  分块并发、断点续传、限速、字幕
src/naming.rs    命名模板
src/table.rs     预览表格渲染（按中文双列宽对齐）
src/tui.rs       交互界面（ratatui）
src/manager.rs   后台下载队列（每服务器一条下载流）
src/model.rs     API 数据模型
install.sh       一键安装脚本
.github/workflows/  CI 与 tag 发布
```
