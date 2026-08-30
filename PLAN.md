# qaqh-webui 重写计划（PLAN.md）

> 目标：在空仓库上重写浏览器 webUI（全新 UI，不继承旧 renderer 代码）。
> 本文档是重写任务的执行总纲；协议纪律以 `QAQ-Harness/docs/webui-development-standard.md`
> （MUST/NEVER，对人类与 LLM agent 同等约束）与 `docs/frontend-contract.md` 为准，
> 冲突时以后端代码 > frontend-contract > 本文档排序裁决。
>
> **技术栈刻意不在本文档中出现**：框架/构建工具/语言由仓库所有者在 M0 启动前
> 单独指定（见 §1）。执行会话不得自行选型、不得提前脚手架。

## 0. 背景与现状

- 本仓库当前只有 `webui-dist/`（旧 UI 的**构建产物**，从 QAQ-Harness 迁出；
  无源码）。它仅作新 UI 的**行为参照物**，新 UI 上线对齐后删除。
- 后端（QAQ-Harness）已于 2026-08 完成协议瘦身，本计划镜像的是**最终形态**：
  - open 握手无能力矩阵（`schema`/`version`/`client_instance_id`）；
  - 服务面统一为 `POST /ringing/v1/service/{method}`（旧 `/queries` `/actions`
    已删除，**禁止出现对它们的任何调用**）；
  - worker 边界线格式与本仓无关（纯 daemon 内部）。
- daemon 端已实现双形态托管：编译期嵌入（`webui-dist`）+ 磁盘旁路
  （`renderer_root()`：`QAQH_DEBUG_RENDERER_DIR` → `out/renderer` →
  `resources/out/renderer` → exe 旁 `out/renderer`）。

## 1. 技术栈（刻意留白）

本计划只冻结**协议契约（§2）、产物约定、里程碑（§3）与验收标准**；
框架、构建工具、语言选型由仓库所有者在 M0 启动前指定并回填本节。
无论最终选什么栈，以下约束不可协商：

1. **产物输出目录**：构建产物落在 `out/renderer`（build.rs 自动同步与
   安装器收集均以此为准）；
2. **相对路径**：产物内部资源引用必须相对（daemon 在 `/debug/` 前缀下
   托管并注入 `./__qaqh_bridge__.js`）；
3. **开发回路**：支持"改文件 → 浏览器刷新可见"（daemon 从磁盘实时读取
   产物目录；`just dev` 起 daemon）；
4. **SSE 必须手写流解析**（协议 N2）：fetch + ReadableStream，框架/库
   无权代劳，禁止任何封装到 EventSource 的捷径；
5. **类型镜像纪律**（§4）按所选语言等价落地。

## 2. 与 daemon 的对接契约（重写必须逐条满足）

1. **连接**：`POST /ringing/v1/clients/open`，body
   `{schema:"qaqh.Ringing", version:1, client_instance_id:<uuid-v4>}`；
   响应 `{accepted, client_session_id, server_epoch, lease_ttl_ms,
   renew_interval_ms}`；代差 → 426 `unsupported_version`（展示"需更新"，停重试）。
2. **鉴权双头**（每个请求）：`Authorization: Bearer <token>` +
   `X-QAQH-Client-Session-Id: <client_session_id>`；token 只来自
   `window.__QAQH_DEBUG__`（daemon 注入 `./__qaqh_bridge__.js`），**仅内存持有**
   （N3），禁止进 URL/query/日志/storage。
3. **SSE（N2）**：一律 fetch + ReadableStream 手写解析；**禁止 EventSource**。
   三个频道各一条流：`GET /ringing/v1/events/{control|conversation|tool}`，
   `Last-Event-ID: <epoch>:<channel>:<seq>` 断点续传；server 每 15s 发注释行
   keepalive（客户端 idle 判活须按**字节**计，参考后端 45s 阈值）；
   `event: ringing.reset_required` → 全频道重置重放。
4. **命令**：`POST /ringing/v1/commands/{channel}`，envelope 携带
   `command_id`（uuid-v4，幂等键）、`client_instance_id`、`client_session_id`、
   可选 `seed`/`expected_revision`。**会话生命周期只走此面**（N5）；
   服务面不含 `session.new/resume` 等（404）。
5. **timeline（唯一历史真源，N6）**：
   `GET /ringing/v1/sessions/{seed}/bootstrap`（全量快照）+
   `GET /ringing/v1/sessions/{seed}/timeline?before_turn&limit`（分页）+
   `GET /ringing/v1/sessions/{seed}/timeline/events`（SSE，严格 +1 光标；
   gap → 全量 re-baseline，禁止本地"加载更多"猜测）。
6. **服务面**：`POST /ringing/v1/service/{method}`，单一方法表
   `qaqh_runtime::ringing::service_methods`：22 Read + 19 Write；
   Read 子集须带 `seed`；错误码 Read→`query_failed`、Write→`action_failed`、
   未知→404 `unknown_method`。
7. **附件**：`POST /ringing/v1/content`（multipart）上传得
   `ContentRef{content_id, sha256, media_type}`，命令中只传 ref 不传路径。
8. **禁止**：WebSocket/轮询/第二协议（N1）；调用 `/control/v1/stop*`（安装器专用）。
9. **状态机**：`OPENING → READY(leased) → ATTACHED`；续租按
   `renew_interval_ms` 循环，连续失败 → 重新 OPEN；`server_epoch` 变化 →
   全量重建（重新 OPEN → bootstrap → 重放 timeline）。

## 3. 里程碑

| 阶段 | 内容 | 验收 |
|---|---|---|
| M0 | 按所有者指定的技术栈（§1）初始化工程；产出满足：`out/renderer`、相对路径引用、manifest/icons 占位 | 构建产物可被 daemon 托管（拷入产物目录后 `/debug/` 打开 + 桥脚本注入成功） |
| M1 | 传输层：`transport/http`（双头注入 + 401 拦截 + 超时）、`transport/sse`（fetch 流解析 + Last-Event-ID + 退避重连 + 字节级判活） | 单测覆盖：多行 data/CRLF/跨 chunk UTF-8/注释行丢弃/断线续传 |
| M2 | 会话生命周期：open → lease 续租循环 → epoch 重建；provider 抽象（浏览器模式读 `__QAQH_DEBUG__`） | §7 自检 ①② 通过 |
| M3 | timeline 投影：bootstrap + 分页 + timeline SSE → 单会话只读视图 | §7 自检 ④（刷新断点续传不丢消息） |
| M4 | 命令面：发送消息 / 中止（conversation 频道）；工具事件展示（tool 频道） | §7 自检 ③ 通过 |
| M5 | 服务面：会话列表/侧栏、配置读写、工作区（typed 方法常量，禁散落字面量） | 服务面 CRUD 走通 |
| M6 | 交互：ask_user / permission / plan review（control 频道 InteractionRequested） | 挂起回合可答/可驳 |
| M7 | 附件上传 + PWA manifest/icons + 视觉打磨；对齐旧 dist 后删除 `webui-dist/` | §7 全清单 + PWA 可安装 |

## 4. 类型镜像纪律

`protocol/types` 手工镜像 `qaqh-ringing`，文件头注明"改动须对照后端 PR"；
事件变体按需增量镜像，不必一次搬全 82 个；服务方法名一律来自
`protocol/methods` 的 typed 常量（对照 `service_methods.rs` 生成）。
**单一来源**：全部 import 自 protocol/，禁止散落字面量。

## 5. 与后端/安装器的衔接（非本仓职责，登记依赖）

- `QAQ-Harness/crates/qaqh-daemon/build.rs`：`out/renderer` 存在时自动同步进
  `webui-dist` 嵌入；缺失则占位——单文件分发路径零干预。
- winui 安装器（`collect-payload-winui.ps1`）：待补 renderer 目录收集 +
  存在性硬校验（同目录双件形态，`frontend-contract.md` §4 布局）；
  后续 qaqh-pack 增量更新加 `webui` kind 实现独立发版。
- 启动器：后端侧薄 exe `qaqh-web`（读 daemon.json → 探活 → 开浏览器），
  与本仓无代码耦合。

## 6. 仓库约定

- `webui-dist/`（旧产物）：只读参照，M7 后删除；
- `.gitignore`：构建输出 `out/` 与所选技术栈的依赖目录不入库；
- 分支：`main` 直推（单人仓），里程碑各一提交；
- 验收：每个里程碑跑一遍
  `QAQ-Harness/docs/webui-development-standard.md` §7 自检清单（curl 驱动）。
