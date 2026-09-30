# qaqh-tui

QAQ-Harness 的终端前端（ratatui + tokio），基于 `qaqh.Ringing` v2 单流协议直连本地 daemon。
当前唯一 UI 是 **V2 Fullscreen**；旧 TUI v1 全屏与 V2 inline 设计已删除。

## 运行

```bash
cargo build --release
# daemon 未运行时会尝试从 QAQH_BACKEND_ROOT/target/debug 等候选路径拉起 `qaqh-daemon run`
./target/release/qaqh-tui.exe

# 自检：发现 → 存活 → /health → open 握手
./target/release/qaqh-tui.exe doctor

# 浏览当前 cwd 下的会话
./target/release/qaqh-tui.exe resume
```

环境变量：`QAQH_DATA_DIR`（数据目录覆盖，默认 `%USERPROFILE%\.qaqh`）、
`QAQH_BACKEND_ROOT`（daemon 拉起候选根）、`QAQH_THEME`（`night` / `day` /
`terminal` / `auto`）、`QAQH_TUI_LOG`（诊断日志落盘路径，见下「排障」）。
`NO_COLOR` 存在时不输出颜色，只保留 glyph 与修饰符。
Bearer token 只从 `daemon.json` 读入内存，永不落日志/URL。

### 排障：抓客户端诊断日志

```bash
QAQH_TUI_LOG=/tmp/qaqh-tui.log cargo run
tail -f /tmp/qaqh-tui.log
```

TUI 自身默认不落日志。设了 `QAQH_TUI_LOG=<path>` 后才安装一个极简文件 logger，
把 `qaqh-client` 的诊断（timeline 重连原因、快照恢复失败、非法 cursor 告警……）
按 `[LEVEL] msg` **追加**写入该文件；不设置时 `log` 门面是空操作，行为与之前一致。
真机排查「timeline 断开 / 重连」「会话恢复为空」这类问题**先开它**——UI 上只剩
一句「断开，1000ms 后重连」，具体原因只在日志里（`ReconnectReason` 只覆盖服务端
主动终止流，普通 HTTP 错误如 401 不带 reason）。

### V2 Fullscreen

```bash
cargo run
```

连接 daemon，复用 Runtime/App 状态。全屏 shell 在 alternate screen 中自持 transcript
视口、滚动位置与渲染缓存；composer、slash 菜单、thinking、status、shortcuts 固定在底部。
Workspace/Modal、会话选择器、设置、权限/ask/plan 共用 v2 路由层。

鼠标能力已覆盖：

- 滚轮滚动、回到底部、滚动条轨道点击与 thumb 拖动
- 助手消息菜单：复制 Markdown、撤销
- permission / ask / plan 按钮 hover / 按下 / 点击
- Settings 行点击

Fullscreen 会启用鼠标捕获；需要终端原生选择/复制时按住 `Shift` 拖选（tmux
中同样用 `Shift` 绕过应用的 mouse tracking，或使用 `prefix + [` 的 copy-mode）。

`--no-spawn` 只连接已有 daemon，不自动拉起。旧的 `--v1` 与
`--v2-inline` 入口会明确报错退出。

ask_user 采用阻塞式单题分页：`←/→` 切题，`↑/↓` 移动选项，`Enter`
选择并前进，`Space` 只选择，`1-9/a-f` 直接选择，`e`/`z` 输入自定义答案，
`Esc` 跳过。快捷键与屏幕编号统一为 1-based。

## 界面与按键

| 区域 | 说明 |
|---|---|
| transcript | timeline 权威投影：回合 → 块（text / reasoning 可展开 / 工具卡）；顶部“加载更早消息”或 PgUp 分页，`Alt+T` 展开最近历史思考，`Alt+E` 切换最近工具卡 |
| Workspace | F4 打开 todo / 最近改动 / 会话 / 设置 / 帮助工作区 |
| composer | Enter 发送 · Ctrl+P 模式 · Ctrl+A 附件 · Ctrl+Y 撤销回合 · Ctrl+E 压缩 |
| 状态栏 | 连接相位 + epoch · toast · token 用量与上下文占比 · 活动 · 时钟 |

全局：Ctrl+T 思考回放（当前回合）· Ctrl+N 新建会话 · Alt+W 关闭标签（会话保留）·
Alt+T 展开/收起最近历史思考 · Alt+E 展开/收起最近工具卡 ·
Ctrl+L 会话列表（恢复/归档/删除，D 删除需确认）· Ctrl+, 配置面板 · F1 帮助 ·
Ctrl+C×2 / Ctrl+Q 退出。
思考回放浮层：↑↓/PgUp/PgDn 滚动 · `e` 交给 `$PAGER` 全文浏览（默认 `less -R`）· Esc 关闭。

Slash：`/new [cwd]` 新建 · `/help` 帮助 · `/clear` 清空输入 ·
`/export [path]` 导出当前会话为 Markdown（默认写入当前目录，含回合头/工具卡/思考聚合；
更早回合的折叠/归档状态如实标注）。

### 子代理实时观测（Ctrl+↑/↓）

父会话通过 `spawn_subagent` 拉起的子代理来自后端 `TeamSnapshot/TeamDelta`
投影；roster 以稳定 `AgentPath` 为键，`unloaded` 条目仍保留。`control.subagents`
只用于 bootstrap 时建立 child timeline attach，不再从工具卡 JSON 推导身份。

- `Ctrl+↑`：深入最近拉起的子代理；再次按下在子代理间循环切换
- `Ctrl+↓` / `Esc`：返回父会话
- `/subagents`：查看 roster、inbox、path prefix 过滤，并打开 child transcript
- 观测中 transcript 显示子代理实时 transcript（任务/思考/工具/作答），
  composer 折叠为只读提示条；PgUp/PgDn/Ctrl+Home/End 滚动
- 观测为只读：子代理无人值守运行，终态结果自动注入父会话；
  会话关闭后本地保留最后快照，卸载不等于删除

交互弹窗（优先级 permission > ask > plan）：工具权限 `a` 批准 / `d` 拒绝 /
`t` 信任目录（高风险+路径时）；ask `1-9` 选项、`e` 自定义输入、`Esc` 跳过；
plan review `a` 批准 / `g` 批准+自主 / `r` 拒绝（输入理由）。

## 架构

本仓**不含任何传输层与协议视图**：连接生命周期、SSE 流、cursor/reset/rebase、
发现与拉起、服务面与内容面全部由 `qaqh-client` 承担；协议类型（wire + 领域投影）
一律从那里 import。本仓只写「状态机 + 视图」。

```
src/
  main.rs        CLI（doctor / resume / 默认全屏）+ QAQH_TUI_LOG 文件 logger
  runtime.rs     qaqh-client ↔ RuntimeMsg 适配：ClientHandlers 回调转投、
                 会话集合 diff 成 activate/deactivate timeline、连接相位订阅
  protocol/      只剩两个 re-export 面：qaqh-config-api 的配置读写模型；
                 历史手工镜像（9 文件 2481 行）已按类型镜像纪律全部删除
  app/           App 状态机 + 投影消费 + 渲染 IR
    ringing_v2.rs  canonical v2 会话状态机：epoch/log/cursor 单调性、重复与
                   过期事件丢弃、交互身份第一答复胜出、driver seat 单调交接。
                   **不持有协议类型**（全部来自 qaqh-client）
    timeline_model.rs  timeline reducer（幂等可重放）+ 压缩分隔锚 + 权威墙钟
    session.rs     单会话 UI 状态（transcript/滚动/composer/挂起交互/dashboard）
    team.rs        TeamSnapshot/TeamDelta roster + inbox 归并（AgentPath 为主键）
    subagent.rs    子代理 timeline 跟踪与视图栈导航（Ctrl+↑/↓）
    export.rs      Markdown 导出
  ui/v2/         v2 视图模型与渲染：adapter（timeline → block）/ transcript /
                 markdown / hit（命中测试）/ modal / workspace / route / theme
  terminal/      fullscreen shell、指针事件、命中区域登记
```

## 协议面（全部经 `qaqh-client`，本仓不自建）

1. 连接：`POST /ringing/v2/clients/open` + `POST /ringing/v2/leases/renew`
   （`renew_interval_ms` 循环，连续失败 → 重新 open）；
2. 每会话**一条** canonical 单流：`GET /ringing/v2/sessions/{session_id}/events`，
   带 `stream_key` 由客户端 demux；open → bootstrap → subscribe，快照不进流；
3. 快照：`GET /ringing/v2/sessions/{session_id}/bootstrap`（typed 三频道）；
4. 命令：`POST /ringing/v2/commands/...`（uuid-v4 `command_id` 幂等键），
   命令状态按 `command_id` 查询；**会话生命周期只走命令面**；
5. timeline（唯一历史真源）：`/timeline` 分页用 **`before_index` 排他游标**
   （全局回合序号 `turn_index`，归档深翻页同样回填）+ `/timeline/events` SSE
   严格 +1，gap 一律 re-baseline；
6. 服务面 `POST /ringing/v2/service/{method}`：Read 带 `session_id`，错误码
   `query_failed` / `action_failed` / `unknown_method`；方法名来自 `QueryRequest` /
   `ActionRequest` 枚举，本仓不留方法名常量表；
7. 团队：`GET /ringing/v2/sessions/{session_id}/team`（roster/inbox/board，
   roster 以稳定 `AgentPath` 为键）；
8. 内容面 `GET /ringing/v2/content/{content_id}`（交互正文按 `ContentValue::Ref`
   取回；命令只传引用不传路径）；
9. 禁 WebSocket / 轮询；不调用 `/control/v1/stop*`（安装器专用）。

跨仓契约由 `scripts/static-gates.sh` 变成可执行检查（G1 依赖面 / G2 展示层不手解
JSON / G3 不读服务端存储布局 / G4 reducer 不引渲染层类型 / G5 状态机不重建协议词汇）。

## 协议权威化（2026-09-30）

TUI 曾自带六份手抄协议镜像（`Delivery` / `InteractionKind` / `ResetReason` /
`PendingInteraction` / `DriverState` / `BootstrapSnapshot`）与三个适配器。
漂移**已经真实发生且失败模式是静默的**（`seed`→`session_id`、镜像漏 `request`
正文；信封新增 `ts_ms` 时以编译失败暴露）。现已整刀删除，并同时接上后端新增面：

- **`ts_ms`（信封携带的源 fact 墙钟）**：transcript 里用户回合头的时间戳直接来自
  它；缺席（合成 ephemeral 事件）时**什么都不画**，不用本地时钟兜底；
- **`CompactionApplied`**：此前被 `=> {}` 丢弃，现在在事件到达时的最后一个回合之后
  渲染「此前已压缩」分隔（与 webui W3 同口径；快照重载清除，锚点被淘汰则顶到最前）；
- **内部身份统一为 `session_id`**：`SessionState`/`StreamKey`/`RuntimeMsg` 等约 900
  处 `seed` 命名全部改名，只剩两处**故意保留的 legacy 引用**（`reset_from_legacy_seed_key`
  与其 `"seed"` 键，用于证明旧键名不再被接受）。

测试夹具统一走 `qaqh-client` 权威类型的反序列化（`src/app/v2_fixtures.rs`），
后端加必填字段当场报错、加可选字段走上游 `#[serde(default)]`，不再有手抄信封。

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
- **回合内存不设硬上限**：内存边界由两层机制提供——后端 seal 后 offload 壳化
  （正文截断 + 全文落 sidecar）与前端 `SegmentCache` 只保视口附近段。历史上的
  400 回合计数切片已删除（整回合消失违反「丢弃必须可见」，且是性能悬崖）；
  `cap_turns` 作为兜底能力保留但**无生产调用者**；
- **会话隔离**：transcript/滚动/composer/挂起交互/dashboard 全部按 `session_id`
  持有（`SessionState`），全局仅连接级身份与 toast 共享；每会话一条 canonical
  单流互不串扰。

## workspace 侧栏与 todo 的数据面

todo 是**领域状态**，不是事件流——侧栏直接消费状态面，transcript 里的 todo
工具卡只是历史轨迹，二者互补：

- bootstrap control state 内置 `dashboard_snapshot`（打开标签页即有初值）；
- agent 调 `todo` 工具时 daemon 即时推送 `DashboardSnapshot`（replaceable，
  含 `tasks[{id,subject,description,status,evidence}]` + `current_todo_id` +
  `recent_edits`，engine_tool.rs "Instant refresh for todo tools"）；
- `todo.status {session_id}` / `session.dashboard {session_id}` 为拉取兜底
  （当前未轮询，遵循事件驱动纪律）。

## 协议纪律对照（PLAN.md §2，2026-09-30 重写）

> ⚠ PLAN.md 是 **qaqh-webui** 仓的重写计划，随本仓初始化一并带入，**不是本仓的
> 权威来源**。下表按本仓现状重写；每条都对得上 `static-gates.sh` 的可执行检查
> 或 `qaqh-client` 的实现。

1. 传输与握手：`qaqh-client` 负责 `open`（`{schema,version,client_instance_id}`）、
   版本代差（426 `unsupported_version` 不重试）、续租与租约重协商；本仓不实现 HTTP/SSE；
2. 双头鉴权（`Authorization` + `X-QAQH-Client-Session-Id`）由 `qaqh-client` 注入，
   token 仅内存、不进 URL/日志/storage；
3. 事件面是**每会话一条** canonical v2 单流（`/ringing/v2/sessions/{id}/events`），
   带 `stream_key` demux，不是三条频道流；cursor 是 canonical token，
   `ringing.reset_required` → 重 bootstrap（状态机先标记 pending、旧 UI 保留）；
4. 命令走 `POST /ringing/v2/commands/*`，uuid-v4 `command_id` 幂等键，
   终态按 `command_id` 查询；**会话生命周期只走命令面**；
5. timeline 分页用 `before_index`（排他、全局回合序号）+ timeline SSE 严格 +1，
   gap 一律 re-baseline；`has_more` / `truncated_before` 分别表达「还能翻」与
   「翻到头但历史更长」；
6. 服务面 `POST /ringing/v2/service/{method}`：方法名来自 `QueryRequest` /
   `ActionRequest` 枚举（本仓无方法名常量表），Read 带 `session_id`，错误码
   `query_failed` / `action_failed` / `unknown_method`；
7. 内容面 `POST /ringing/v2/content` 上传 → `ContentRef`，`GET /ringing/v2/content/{id}`
   下载；命令只传引用不传路径，下载校验 sha256；
8. 禁 WebSocket / 轮询 / 第二协议；不调用 `/control/v1/stop*`（安装器专用）；
9. 展示层只消费 typed 投影（G2），不手解 JSON、不读服务端存储布局（G3），
   reducer 不引入渲染层类型（G4），状态机不重建协议词汇（G5）。

## 相对 winui 侧的修正（以忠于后端为准）

- 新会话发现：消费 `ControlDelta::SessionCreated` 的信封 `causation_id == command_id`
  关联，不做 15s 列表轮询 diff；
- 不读服务端存储布局（磁盘旁路），元数据全部走 `session.list` / bootstrap；
- 附件上传失败显式 toast（winui 静默吞错）；
- envelope 级 `session_id`（与命令体 `session_id` 是两个独立字段，后端 validate
  强制）——经真实 daemon 实测修正；
- canonical 单流的光标/reset 由 `qaqh-client` 承担，本仓只在状态机里拒绝
  重复/过期/跨 epoch 事件；timeline gap → 快照 re-baseline（对齐 `qaqh-client`
  参考实现，而非 winui 的 15s 停滞检测兜底）；
- 重新 open（同 epoch）后对所有打开的会话重 attach + re-bootstrap（租约条目按
  client_session_id 记录，重开即失效）；
- timeline 翻页用 prepend 合并（保留已加载窗口），游标是权威 `turn_index`；
  undo 走 receipt 轮询后重取。

## 已知边界 / 未来工作

- markdown 为纯文本渲染（无 md 解析；后续可接 markdown-core 风格的 TUI 渲染）；
- 内联 base64 图片（`images: Vec<ImageBlock>`）未启用，附件走 content 上传；
- `expected_revision` 乐观并发目前仅 skills.operation 场景可用，命令信封字段
  已由 `qaqh-client` 承载但未主动使用；
- transcript 的墙钟只在**用户回合头**渲染；工具卡/助手块的时间未上屏
  （权威值已在 `TranscriptBlock.at_ms` 上，需要时可直接消费）；
- 压缩分隔是「事件到达时刻」的位置锚（与 webui 同口径），不是按
  `replaces_through_fact_seq` 反查历史位置——后者要等后端提供可映射的
  timeline 事实；
- Windows IME 组合输入依赖终端自身行为（crossterm 无合成事件）。
