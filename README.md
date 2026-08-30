# qaqh-tui

QAQ-Harness 的终端前端（ratatui + tokio），基于 `qaqh.Ringing` v1 协议直连本地 daemon。

## 运行

```bash
cargo build --release
# daemon 未运行时会尝试从 QAQH_BACKEND_ROOT/target/debug 等候选路径拉起 `qaqh-daemon run`
./target/release/qaqh-tui.exe

# 自检：发现 → 存活 → /health → open 握手
./target/release/qaqh-tui.exe doctor
```

环境变量：`QAQH_DATA_DIR`（数据目录覆盖，默认 `%USERPROFILE%\.qaqh`）、
`QAQH_BACKEND_ROOT`（daemon 拉起候选根）。Bearer token 只从 `daemon.json`
读入内存，永不落日志/URL。

## 界面与按键

| 区域 | 说明 |
|---|---|
| 标签栏 | 多会话 tab；`!`＝挂起交互、`…`＝流式中；Alt+1..9 / Alt+←→ 切换 |
| 会话信息 | model / plan·code 模式 / 代码增删 / seed |
| transcript | timeline 权威投影：回合 → 块（text / reasoning 折叠 / 工具卡） |
| workspace 侧栏 | todo 列表 + 最近改动（DashboardSnapshot 活状态；F4 开关，<100 列自动隐藏） |
| composer | Enter 发送 · Ctrl+P 模式 · Ctrl+A 附件 · Ctrl+Y 撤销回合 · Ctrl+E 压缩 |
| 状态栏 | 连接相位 + epoch · toast · token 用量与上下文占比 · 活动 · 时钟 |

全局：Ctrl+T 新建会话 · Ctrl+W 关闭标签（会话保留）· Ctrl+L 会话列表
（恢复/归档/删除，D 删除需确认）· Ctrl+, 配置面板 · F1 帮助 · F3 展开思考 ·
Ctrl+C×2 / Ctrl+Q 退出。

交互弹窗（优先级 permission > ask > plan）：工具权限 `a` 批准 / `d` 拒绝 /
`t` 信任目录（高风险+路径时）；ask `1-9` 选项、`e` 自定义输入、`Esc` 跳过；
plan review `a` 批准 / `g` 批准+自主 / `r` 拒绝（输入理由）。

## 架构

```
src/
  protocol/    类型镜像层（PLAN.md §4：手工镜像 qaqh-ringing/qaqh-domain，改动须对照后端 PR）
    mod.rs       RINGING_SCHEMA/VERSION、Channel、Delivery、安全整数
    capability.rs  open 握手 + leases/renew
    envelope.rs    Ringing{Event,Command}Envelope、ack、receipt、reset_required
    command.rs     三频道命令（13+6+2 变体全量镜像）
    event.rs       三频道事件（13+7+16 变体全量镜像）+ UsageInfo/ContentRef/ToolResult
    timeline.rs    timeline 快照/条目（唯一历史真源 N6）
    snapshot.rs    bootstrap 三频道快照 + 宽松解析视图
    methods.rs     服务面 22 Read + 19 Write 方法名常量 + 方法表
  transport/
    discovery.rs   daemon.json 发现 + pid 存活 + detached 拉起 + 120ms 轮询
    http.rs        双头注入（Bearer + X-QAQH-Client-Session-Id）、错误分类、
                   bootstrap/timeline/commands/service/content 端点、手写 multipart
    sse.rs         手写 SSE 帧解码（严格 UTF-8、CRLF、注释行、多 data 行）+ 光标解析
  runtime.rs     open/续租循环（renew_interval/2，2 次失败重 open）+ 三频道 SSE 流 +
                 per-seed timeline 流（严格 +1，gap/reset/epoch 变化 → 快照 re-baseline）
  app/           App 状态机 + timeline reducer（幂等）+ 渲染 IR
  ui/            ratatui 0.30 视图（标签栏/对话/弹窗/覆盖层/状态栏）
```

## 多会话内存管理（参照 opencode v2 TUI 的分析结果）

opencode v2（`anomalyco/opencode` 的 `packages/tui`，SolidJS + Bun）采用
单活动路由（home/session/plugin，无 tab strip）+ `/sessions` 弹窗切换；
其数据层 `sync()` 进入会话时并行拉取 session/messages(limit=100)/todo/diff，
并做**滑动窗口裁剪**（每会话仅保留最近 100 条消息，窗口外连同 parts 一并删除），
todo 经 `todo.updated` 事件按 sessionID 入 store。但其全局 store 中的历史消息
**从不回收**。本 TUI 在其基础上做了更进一步的内存纪律：

- **渲染缓存只保 active**：非 active 标签的 `RenderedTranscript` 一律丢弃，
  聚焦时按需重建（宽度键控，一次渲染成本）；
- **LRU transcript 逐出**：保留最近 4 个焦点标签的 timeline 模型，超出者
  只存轻状态（标题/用量/挂起交互/dashboard），transcript 丢弃并标记
  `needs_rebaseline`，重新聚焦时自动 re-baseline（服务端是权威历史）；
- **回合滑动窗口**：单会话内存中最多 400 回合（`cap_turns`），超出丢最旧并
  置 `has_more`，PgUp 加载更早仍可用（before_turn 锚点自适应）；
- **会话隔离**：transcript/滚动/composer/挂起交互/dashboard 全部 per-seed
  持有（`SessionState`），全局仅连接级身份与 toast 共享；per-seed timeline
  流互不串扰，控制面三频道按信封 seed 路由。

## workspace 侧栏与 todo 的数据面

todo 是**领域状态**，不是事件流——侧栏直接消费状态面，transcript 里的 todo
工具卡只是历史轨迹，二者互补：

- bootstrap control state 内置 `dashboard_snapshot`（打开标签页即有初值）；
- agent 调 `todo` 工具时 daemon 即时推送 `DashboardSnapshot`（replaceable，
  含 `tasks[{id,subject,description,status,evidence}]` + `current_todo_id` +
  `recent_edits`，engine_tool.rs "Instant refresh for todo tools"）；
- `todo.status {seed}` / `session.dashboard {seed}` 为拉取兜底（当前未轮询，
  遵循事件驱动纪律）。

## 协议纪律对照（PLAN.md §2）

1. open 握手 `{schema,version,client_instance_id}`，426 `unsupported_version` → 停止重试；
2. 双头鉴权每个请求；token 仅内存；
3. SSE 手写流解析（无 EventSource 类库）；`Last-Event-ID: <epoch>:<channel>:<seq>`；
   45s 字节级判活；`ringing.reset_required` → 重取快照；
4. 命令走 `POST /ringing/v1/commands/{channel}`，uuid-v4 `command_id` 幂等键；
   **会话生命周期只走命令面**（服务面不含 session.new/resume）；
5. timeline bootstrap + `?before_turn&limit` 分页 + timeline SSE 严格 +1，gap 一律
   re-baseline；
6. 服务面 `POST /ringing/v1/service/{method}` 方法名全部来自 `protocol::methods`
   常量；Read 带 `seed`（envelope/参数双级）；错误码 `query_failed`/`action_failed`/
   `unknown_method`；
7. 附件 `POST /ringing/v1/content`（seed/media_type/content 三字段 multipart）→
   `ContentRef`，命令只传引用不传路径；下载校验 sha256；
8. 禁 WebSocket/轮询；不调用 `/control/v1/stop*`（安装器专用）。

## 相对 winui 侧的修正（以忠于后端为准）

- 新会话发现：消费 `SessionStateChanged{created}` 的信封 `causation_id == command_id`
  关联，不做 15s 列表轮询 diff；
- 不读 `sessions/{seed}/meta.json` 磁盘旁路，元数据全部走 `session.list`/bootstrap；
- 附件上传失败显式 toast（winui 静默吞错）；
- envelope 级 `seed`（与命令体 seed 是两个独立字段，后端 validate 强制）——经
  真实 daemon 实测修正；
- 频道 SSE 光标与帧 id 严格比对，失配重连；timeline gap → 快照 re-baseline
  （对齐 `qaqh-client` 参考实现，而非 winui 的 15s 停滞检测兜底）；
- 重 open（同 epoch）后对所有打开的会话重 attach + re-bootstrap（租约条目按
  client_session_id 记录，重开即失效）；
- timeline 翻页用 prepend 合并（保留已加载窗口），undo 走 receipt 轮询后重取。

## 已知边界 / 未来工作

- markdown 为纯文本渲染（无 md 解析；后续可接 markdown-core 风格的 TUI 渲染）；
- 内联 base64 图片（`images: Vec<ImageBlock>`）未启用，附件走 content 上传；
- `expected_revision` 乐观并发目前仅 skills.operation 场景可用，命令信封字段已
  镜像但未主动使用；
- Windows IME 组合输入依赖终端自身行为（crossterm 无合成事件）。
