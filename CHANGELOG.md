# 变更记录

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循语义化版本。
发布时 tag 需与 `Cargo.toml` 中的版本一致（如 `v0.0.3` ↔ `0.0.3`）。

## [0.0.3] - 2026-10-30

### 新增

- 后台下载服务（`embydown daemon`）：TUI 通过 `$XDG_RUNTIME_DIR/embydown/daemon.sock` 控制独立进程，
  关闭 TUI / 终端、`Ctrl+C` 或 TUI 崩溃都不会中断下载；再次打开 TUI 可继续查看进度。
- 队列持久化 `queue.json`：守护进程重启后自动恢复；退出/崩溃时运行中的任务会回到队首，
  配合 `.part` 断点续传继续下载。
- 永久下载记录 `history.jsonl` 与 `embydown history [--limit N]`：完成/失败/取消都会追加记录，
  失败记录包含原因；`--limit 0` 显示全部。
- HTTP 429 限流处理：按 `Retry-After` 自动等待并重试；预计超过 5 分钟的限流会中止本次任务并提示解除时间，
  `.part` 与状态文件保留，到点重跑即可续传。
- TUI 下载页显示失败原因（包括限流解除时间）与后台服务连接错误。

### 变更

- 分块布局只由文件大小决定（默认约 8 块，单块上限 512 MiB），与 `-c` 并发数解耦：
  为避开限流临时调低并发不会让已有进度作废。
- 默认每文件 2 个连接、同时下载 2 个文件（`-c 2 -j 2`）。
- 升级前用其它 `-c` 下载的 `.part.json` 会沿用原有块布局，保住已有进度。
- 新增 `src/daemon.rs`（后台服务与 socket 协议）和 `src/store.rs`（队列/记录持久化）。
- CI：`actions/checkout`、`Swatinem/rust-cache`、`softprops/action-gh-release` 升级到最新版本。

### 修复

- 搜索框、编辑框用 `Backspace` 删除中文时，宽字符右半格可能残留“方框”：
  输入行现在整行强制重绘，确保右半格被空格覆盖。
- `embydown history` 的服务器列按显示宽度对齐，中文服务器名不再错位。
- 版本号更新为 `0.0.3`。

## [0.0.2] - 2026-10-29

### 变更

- 下载：取消/续传/覆盖的完成判定与原子落盘，拒绝 HTML 错误页，增加读超时与可取消等待。
- 并发：同一目标路径串行化，避免多个队列写坏同一个 `.part`。
- 安全：错误链中的 `api_key` 脱敏；锁定版本时禁用无法指定媒体源的回退接口。
- 配置：原子写入 + 跨进程锁；TUI 设置持久化；CLI 复用配置默认值；支持 `~` 展开。
- 命名/字幕：版本目录去重、直链命名、字幕扩展名与同语言多轨处理。
- 移除转码支持。

## [0.0.1] - 2026-10-29

### 新增

- ratatui 交互界面：搜索 / 浏览 / 下载 / 服务器 / 设置。
- 后台下载队列，每个服务器一条独立下载流，下载时界面不退出。
- 多版本归类与单版本干净命名；季目录使用 5 位编号。
- 一键安装脚本 `install.sh`。
- GitHub Actions：CI 与 tag 发布 Release。

[0.0.3]: https://github.com/vansour/embydown/compare/v0.0.2...v0.0.3
[0.0.2]: https://github.com/vansour/embydown/compare/v0.0.1...v0.0.2
[0.0.1]: https://github.com/vansour/embydown/releases/tag/v0.0.1
