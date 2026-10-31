# embydown

[![CI](https://github.com/vansour/embydown/actions/workflows/ci.yml/badge.svg)](https://github.com/vansour/embydown/actions/workflows/ci.yml)
[![Release](https://github.com/vansour/embydown/actions/workflows/release.yml/badge.svg)](https://github.com/vansour/embydown/actions/workflows/release.yml)

用 Rust 写的 Emby 媒体下载工具。**默认走直链**（`/Videos/{id}/stream?static=true`，失败自动回退官方 `/Items/{id}/Download`），支持分块并发、断点续传、整季批量、外挂字幕。

不带任何子命令直接运行 `embydown` 会进入**命令行交互模式**（数字菜单）；`embydown ui` 等价。

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

首次安装使用上面的脚本即可。**安装一次后，后续升级直接用 `embydown update`，不再需要 curl：**

```bash
embydown update                 # 更新到最新正式版
embydown update --check         # 只检查版本，不下载
embydown update --version v0.0.7
embydown update --force         # 已是最新版本也重新安装
```

- 从 GitHub Releases 下载当前架构的裸二进制，校验 SHA-256 后原子替换当前可执行文件
- 安装在 `/usr/local/bin` 等目录时，需要对该目录有写权限（用 `sudo embydown update`）
- 更新后会自动停止旧的后台下载服务；队列与 `.part` 断点保留，下次运行自动续传
- 可通过 `EMBYDOWN_RELEASE_BASE=https://mirror.example.com/embydown/releases/download`
  指向镜像（配合 `--version` 使用）

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

## 命令行交互模式

```bash
embydown              # 直接进入
embydown ui           # 等价写法（也可以写 embydown interactive）
```

运行后显示数字菜单：

| 菜单 | 功能 |
|---|---|
| 1 搜索并下载 | 跨服务器搜索：先选影片、再选来源；也接受 `http(s)://` 直链 |
| 2 浏览媒体库 | 服务器 → 媒体库 → 剧集 / 季 / 分集逐层浏览，选中条目下载；`d` 下载当前目录 |
| 3 下载队列 | 每秒实时刷新进度；剧集按季聚合显示；可暂停/继续、取消单个任务、重试失败任务、取消队列、清空记录 |
| 4 服务器管理 | 添加登录（账号密码 / API Key）、删除、设置搜索范围 |
| 5 下载设置 | 临时文件夹、最终文件夹、连接数、每服务器并发、限速、字幕、覆盖、续传、命名模板（修改后立即保存） |
| 6 未完成任务找回 | 递归扫描 `.part` / `.part.json`，搜索来源并写回原路径续传 |
| 7 下载记录 | 查看历史记录（最新在前） |
| 8 本地审查 | 检查临时/最终文件夹的内容、缺集、异常文件与同步差异（只读目录项，不打开文件） |
| 9 同步到最终文件夹 | 把临时文件夹复制到最终文件夹（会先预览） |

菜单里直接回车会取消当前操作或返回上一级，`0` 返回上一级 / 退出，`Ctrl+C` 随时退出。
输入行支持左右键编辑、上下键翻历史，历史保存在配置目录的 `input_history.txt`。
界面使用颜色区分标题、快捷键与状态；设置环境变量 `NO_COLOR=1` 可关闭颜色。

搜索结果按**相关度**排序：标题完全匹配 > 前缀匹配 > 包含匹配 > 剧名/路径匹配；
同一部影片在不同服务器上的结果会先聚合成一行，同分时短标题优先，剧集按季集排序。

搜索分两级选择，避免多个服务器的重复条目挤在一起：

1. **选择影片**：列表中每行是一部影片/一集，显示类型、年份、季集、覆盖的服务器与来源数量。
   输入 `b` 可重新搜索。
2. **选择来源**：选中影片后列出它的全部来源，逐行显示服务器、版本（多版本显示数量）、
   体积、分辨率/编码/容器；**即使只有一个来源也会列出**。输入 `b` 返回影片列表，
   输入 `r` 重新搜索。

所有服务器会**并发搜索**，单个服务器超时或失败不会阻塞其他结果。
选定来源后会先解析条目，再按需选择季和版本，最后确认加入后台下载队列；
加入后会询问是否继续从当前搜索结果中选择。

「下载设置」的修改会立即写入配置文件（与登录信息同一个 `config.json`），重启交互模式后仍然生效；
`logout` 只清除登录信息，不会重置这些下载设置。限速输入 `-` 表示清除；
最终文件夹与命名模板输入 `-` 分别表示清除和恢复内置默认。

命令行 `embydown dl` 也把这些设置作为默认值（临时文件夹、连接数、并发、限速、字幕、覆盖、续传、命名模板），
命令行显式传参时以命令行为准。默认每文件 2 个连接、同时下载 2 个文件（`-c 2 -j 2`）。
需要临时反着覆盖时可用 `--no-overwrite`、`--no-resume`、`--no-subs`、
`--no-limit-rate`；`--pattern ""` 表示本次使用内置默认命名模板。

下载确认会先解析条目：

- 剧集先列出包含的季，可输入 `1,2` 只下载部分季（回车 = 全部）
- 有多个版本时列出各自实际集数、体积、分辨率、容器；用数字选择版本，`a` 下载全部版本
- 版本归类与 CLI 的 `--dry-run` / `--version` 完全一致

确认后回到搜索结果，也可以返回主菜单继续搜索、浏览或查看队列：
任务进入后台队列后每个服务器独立下载，可以继续从刚才的结果中选择其他来源。
每个服务器有独立的队列、并发数和限速器，多个服务器会同时下载、互不阻塞；
同一服务器内的任务按加入顺序排队，并发数由「每服务器并发数」控制。

下载实际由独立的**后台下载服务**执行，所以退出交互模式、`Ctrl+C`、甚至直接关闭终端都不会中断下载：

```bash
embydown          # 重新打开即可看到仍在进行的下载
embydown history  # 查看历史记录（包括交互模式关闭期间完成的）
```

下载队列默认**每秒实时刷新**，按 Enter 进入操作菜单：

- 排队中的剧集按「剧名 + 季」聚合为一行（如 `某剧 S01（36 集）`），不再逐集刷屏；
  行首编号支持区间（如 `1-36`），单集时显示 `S01E02 · 标题`，电影等非剧集仍逐条显示
- `p` 暂停/继续指定服务器，`P` 全部暂停/继续
- `c` 取消单个任务（运行中或排队中），`x` 取消该服务器的队列，`X` 取消全部
- `R` 重试失败的单个任务，`e` 查看完整失败原因
- `C` 清空最近完成记录（同时删除历史文件），`u` 找回未完成任务

取消或退出时已完成的分块会保留，重新加入同一任务即可续传。
暂停会把正在运行的任务放回队首并保留 `.part`，继续后从断点续传；暂停状态会写入 `queue.json`，后台服务重启后仍然保持暂停。

**未完成任务找回**：选择「未完成任务找回」，会递归扫描临时文件夹里的 `.part` / `.part.json`，
从文件名推导关键字（剧集取剧名并解析 `SxxExx`，电影去掉年份/质量标签），
在所有服务器上搜索；剧集结果会自动展开 Series/Season，只匹配同一季集。
选中一个来源后，下载会**写回原来的文件路径**并复用已有 `.part` 续传，不会生成一份新文件。
也可以直接用 CLI 控制（`--server <别名>` 只作用于单个服务器）：

```bash
embydown pause                      # 暂停全部服务器
embydown pause --server media.micu.hk
embydown resume                     # 继续全部
embydown resume --server media.micu.hk
```

## 本地审查与同步

下载默认先落到**临时文件夹**（下载设置中的「临时文件夹」，命令行 `-o`），
确认内容无误后再复制到**最终文件夹**（媒体库目录）。两个目录都在「下载设置」里配置，
最终文件夹默认 `./library`。

```bash
# 审查：内容、缺集、重复集、未完成/异常文件、临时与最终的差异
embydown review
embydown review --temp /data/downloads --final /data/media

# 先预览同步计划，再真正复制
embydown sync --dry-run
embydown sync
embydown sync --force          # 大小相同的文件也重新复制
```

审查完全在本地完成，不访问服务器，也**不会打开文件读取内容**；
只依据目录项（文件名/路径与大小）判断：

- 统计视频 / 字幕 / 元数据 / 图片数量与总大小，列出剧集（含季数）和影片
- 按 `SxxExx` 检查集号连续性，报告缺集（如缺少 E03）与重复集（可能是多版本）
- 检查空文件、过小视频，以及 `.part` / `.part.json` 等未完成文件
- 对比临时 / 最终文件夹，列出待复制、待覆盖、已一致、仅最终有；
  剧集按「剧名 + 季」聚合（如 `S01（24 集，25.5 GiB）`），加 `-v` 展开为逐个文件

同步规则：

- 默认只复制缺失文件和大小不一致的文件；`--force` 连大小相同的一起复制
- `.part` 与异常文件会跳过并列出，修复后再次同步即可
- 网盘/FUSE 挂载出现临时错误（超时、`State not recoverable` 等）时会自动重试一次；
  仍失败会提示稍后重跑，已成功复制和一致的文件不会重复复制
- 写入最终文件夹时先写同目录临时文件再 rename，避免留下半截文件
- 最终文件夹里多出来的文件不会被删除
- 临时文件夹与最终文件夹相同或互相包含时拒绝执行

交互模式主菜单的 `8) 本地审查` 与 `9) 同步到最终文件夹` 对应这两个命令。

## 后台下载服务与记录

交互模式的「下载队列」会自动拉起后台服务（`embydown daemon`），它与交互模式是独立进程、独立进程组：
交互模式退出或关闭终端时，服务会忽略挂断信号继续下载；下次打开交互模式会重新连接并显示进度。

- 单实例：通过 `$XDG_RUNTIME_DIR/embydown/daemon.sock` 通信，`daemon.lock` 记录 PID，避免两个服务同时下载同一批文件
- `queue.json`：当前排队/运行中的任务（含服务器凭证、直链与暂停状态），服务重启时会把运行中的任务放回队首，配合 `.part` 续传
- `history.jsonl`：每次任务结束追加一行，只增不改；任务失败也会记录，包含失败原因
- `daemon.log`：后台服务的输出与错误，启动失败时先看这里
- `input_history.txt`：交互模式的输入历史（搜索关键字、菜单命令），供上下键翻找

队列、记录、输入历史与日志都在 `~/.config/embydown/`（`$XDG_CONFIG_HOME` 可覆盖），权限 `0600`。
查看记录（时间统一为 UTC）：

```bash
embydown history            # 最近 50 条
embydown history --limit 0  # 全部
```

需要在前台观察后台服务时可直接运行 `embydown daemon`（已有实例时会提示并退出）；
停止服务：`kill "$(cat "${XDG_RUNTIME_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}}/embydown/daemon.lock")"`。

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
# 临时文件夹支持 ~ 展开：-o ~/Downloads 会存到 $HOME/Downloads

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

# 后台服务的下载记录（最新在前；--limit 0 显示全部）
embydown history
embydown history --limit 200

# 暂停 / 继续后台下载队列（省略 --server 时为全部服务器）
embydown pause
embydown resume
embydown pause --server media.micu.hk

# 本地审查临时/最终文件夹（缺集、重复集、未完成/异常文件、同步差异）
embydown review
embydown review --temp ./downloads --final ./library

# 同步临时文件夹到最终文件夹（--dry-run 只预览）
embydown sync --dry-run
embydown sync
embydown sync --force

# 自更新（首次安装后不再需要 curl；下载并校验 SHA-256 后替换当前二进制）
embydown update
embydown update --check             # 只检查版本
embydown update --version v0.0.7    # 安装指定版本
embydown update --force             # 已是最新也重新安装
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

探测用 `Range: bytes=0-0`：返回 `206` 说明支持分段，于是并发拉块；返回 `200` 说明不支持，自动退化成单连接流式下载。

分块大小只由文件大小决定（默认切成约 8 块，单块上限 512 MiB），与 `-c` 并发数无关：为避开限流临时调低 `-c` 不会让已有进度作废。`-c` 只控制每个文件同时发几个 Range 请求。

## 断点续传

- 分块模式：每完成一块就写入 `<文件名>.part.json`（记录总大小、块大小、已完成块号）。中断后**重跑同一条命令**即可续传；升级前用其它 `-c` 下载的 `.part.json` 也会按原来的块布局复用。
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

默认（Emby 标准目录结构，临时文件夹下自动分影视/电影、影视/剧集）：

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

版本号在 `Cargo.toml`，变更记录见 [CHANGELOG.md](CHANGELOG.md)；推送 `v*` 标签会触发 `.github/workflows/release.yml`：

- 校验标签与 `Cargo.toml` 版本一致（例如标签 `v0.0.7` 对应版本 `0.0.7`）
- 构建 x86_64 release，打包二进制、README、CHANGELOG 和 install.sh，生成 SHA-256
- 同时上传裸二进制（`embydown-vX.Y.Z-x86_64-unknown-linux-gnu.bin` 与稳定别名）供 `embydown update` 使用
- 创建 GitHub Release 并上传产物

```bash
git tag v0.0.7
git push origin v0.0.7
```

`main` 分支的 push / PR 会运行 `.github/workflows/ci.yml`（clippy、测试、release 构建）。

## 排错

| 现象 | 原因 / 处理 |
|---|---|
| `HTTP 401` | Token 失效；或 API Key 被删。换 Key / 重新登录 |
| `HTTP 403` | 账号没有该媒体的播放/下载权限；用管理员账号或在 Emby 里给该用户开权限 |
| `HTTP 404` | 条目 ID 不对，或直链路径不被该版本支持（工具会自动试下一个候选） |
| `HTTP 416` | 文件被服务端替换，删掉 `.part` 与 `.part.json` 重下 |
| `HTTP 429` / Cloudflare `error code: 1015` | 服务器或 CDN 限流。短时间（≤5 分钟）会自动按 `Retry-After` 等待重试；更长的限流会中止本次任务并提示解除时间，`.part` 会保留，到点重跑同一命令即可续传 |
| `HTTP 404 page not found` | 地址漏了 `https://`，被 301 后登录 POST 降级成 GET。补全 scheme |
| 直链 302 | 部分服务器把直链跳到 `/play/video/{id}` CDN；reqwest 自动跟随且保留 `Range`，无需处理 |
| 卡在探测阶段 | 服务器前面有反代且未透传 `Range` 头；用 `-c 1` 走单连接 |
| 交互模式提示「后台下载服务连接失败」 | 看 `~/.config/embydown/daemon.log`；socket 默认在 `$XDG_RUNTIME_DIR/embydown/`，路径过长时设置较短的 `XDG_RUNTIME_DIR`；也可前台运行 `embydown daemon` 复现 |
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
src/interactive.rs 命令行交互模式（数字菜单）
src/manager.rs   后台下载队列（每服务器一条下载流）
src/daemon.rs    后台下载服务（Unix socket 协议、单实例、自动拉起）
src/store.rs     队列与下载记录持久化（queue.json / history.jsonl）
src/repair.rs    未完成任务扫描、关键字推导与来源匹配
src/review.rs    本地审查与临时 → 最终文件夹同步
src/model.rs     API 数据模型
src/update.rs    自更新（GitHub Releases + SHA-256 + 原子替换）
install.sh       一键安装脚本
CHANGELOG.md     版本变更记录
.github/workflows/  CI 与 tag 发布
```
