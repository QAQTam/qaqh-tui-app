# QAQH TUI v2 交互否定路径 PTY 覆盖报告（M6.3 增量）

> 日期：2026-09-23
> 状态：已完成（TUI 侧）；含两条**后端 plan 钩子保真度**待办
> 锚点：`tui-anchor-2026-09-23-p3` @ `8dbe22e03239e95d19770dacd7b4dd0645ca6a7a`
> 脚本：`scripts/e2e-v2-interactions.sh`（新增 3 个 MODE）
> 关联：后端 issue #41（测试钩子）、issue #42（timeline seq 缺口）

## 0. 结论

M6.3 此前只覆盖了交互的**肯定路径**（`a` 批准 / `1` 作答），拒绝、跳过、驳回三条
否定路径一直没有 harness。本轮补上，并把判据从「模态消失」换成**后端权威证据**：

| MODE | 交互 | 按键 | 后端权威证据 | 结果 |
|---|---|---|---|---|
| `permission-deny` | permission | `d` | timeline 里工具块 `state=failed` + 输出 `[DENIED] …` | ✅ |
| `ask-dismiss` | ask | `Esc` | timeline 回合 `state=cancelled` + 账本 `decision=dismissed` | ✅ |
| `plan-reject` | plan | `r` + 理由 + `Enter` | 会话事实账本 `decision=rejected` | ✅ |

三个模式连同原有 6 个（`permission` / `ask` / `plan` / `pager` /
`permission-hang` / `ask-hang`）**全部 PASS**。

## 1. 为什么不能用「模态消失」当判据

`respond_permission(false)` / `dismiss_ask()` / `respond_plan(false, …)` 的第一步
都是**本地下架面板**（`sess.pending_* = None`），然后才异步发控制命令。也就是说
面板消失**只证明 TUI 本地清了状态**，完全不证明裁决送达后端、更不证明后端按
否定语义处理了它。

所以三条判据全部取自后端：

- **timeline 快照**：`GET /ringing/v1/sessions/{seed}/timeline`（daemon 物化视图）；
- **会话事实账本**：`sessions/*/events.jsonl` 里 daemon 自己 fsync 的
  `interaction_resolved` 事实，用 `decision_ref` **哈希校验**而不是子串匹配。

## 2. permission-deny

按 `d` → `respond_permission(false)` → 后端 `emit_denied`。实测快照：

```json
{"turn_id": "t1", "state": "completed",
 "rounds": [{"round_num": 0, "blocks": [
   {"block_id": "tool:call_permission_1", "kind": "tool",
    "tool": {"name": "exec", "state": "failed",
             "output": "[DENIED] 'exec' (user denied permission)",
             "failure": {"code": "TOOL_EXECUTION_FAILED",
                         "message": "[DENIED] 'exec' (user denied permission)"},
             "permission": {"category": "exec", "level": 2, "risk": "high"}}}]}]}
```

判据：存在 tool 块满足 `state == "failed"` 且输出含 `[DENIED]`。

> 注意：这里**不会**出现 `tool_denied` 失败码。`tool_denied` 走的是
> `emit_timeline_denied` 的「块尚未打开」分支；permission 流程里块在权限门之前
> 已经打开，结果经常规回合路径回写，失败码是 `TOOL_EXECUTION_FAILED`。
> 首版判据写成找 `tool_denied` 是错的，靠实跑纠正。

## 3. ask-dismiss

按 `Esc` → `dismiss_ask()` → `InteractionAskDismiss`。后端
`engine_turn.rs::handle_ask_dismiss` 走 `TurnAborted`，回合落
`TimelineTurnState::Cancelled`；同时 `record_interaction_resolution(…, "dismissed")`
写进账本。

判据（两条都要）：timeline 里出现回合 `state == "cancelled"`，且账本里存在

```text
decision_ref = sha256:928688fb794b939201c343e69780b7f920919f3ecbbf11ff1486115dc5bc6686
```

该哈希是 `{"decision":"dismissed"}` 的 sha256，与后端
`serde_json::to_vec(&json!({"decision": decision}))` 一致——脚本本地复刻该算法
（`decision_ref()`），所以断言是**可校验的指纹**而不是文案匹配。

## 4. plan-reject

按 `r` 进理由输入态，输入独有标记 `e2e-plan-reject-marker`，`Enter` 提交。
判据：账本里存在 `{"decision":"rejected"}` 的指纹

```text
decision_ref = sha256:1860011b8369cb09d7bbac77f2fc8ba0d24887708e62ffd09da764acdf0ce97f
```

**为什么不用 timeline / provider 上下文**——这正是本轮发现的钩子缺口（见 §5）：
`QAQH_TEST_PLAN_REVIEW` 钩子只在控制面发 `PlanReviewRequested`，既不物化 timeline
块（快照里该回合 `rounds: []`），也不把拒绝结果并进模型上下文（后续 provider
请求里只有 `system` + `user` 两条消息）。这两条路都查不到，只剩 daemon 账本。

## 5. 后端待办：`QAQH_TEST_PLAN_REVIEW` 钩子的保真度缺口（2 条）

> 已提 **issue #43**（P2，本仓 issue 面，抄送后端）；在 #41 留了交叉引用。

背景：`engine_tool.rs` 里 `pending_plans` 在工具侧**恒为空**，也就是说当前
plan review **只能**靠 `QAQH_TEST_PLAN_REVIEW` 触发。钩子的保真度直接决定
TUI 侧能验证到哪一步。

### 5.1 钩子不发 timeline intent → 快照里 `rounds: []`

钩子只 `emit_domain(PlanReviewRequested)`，没有对应的 `TurnOpened` /
`BlockOpened(kind=Tool)`。于是同一回合的物化结果是：

```json
{"turn_id": "t1", "state": "completed", "rounds": []}
```

影响：TUI 无法在契约测试里验证 plan 块的**转写渲染**（tool 卡片、plan 正文、
拒绝后的 `Plan rejected: …` 结果文本），只能验证 modal 本身。

### 5.2 拒绝结果成为孤儿 tool 消息 → 不进模型上下文

`handle_plan_response` 对 `approved=false` 会
`push_tool_result_direct(call_id, "Plan rejected: <理由>\n\n<正文>", false)`。
钩子场景下**没有**对应的 assistant `tool_calls` 消息（plan 是凭空注入的），
这条 tool 结果成了孤儿，实测下一轮 provider 请求只有：

```text
roles = ['system', 'user']   # 没有 assistant tool_calls，也没有 tool 结果
```

影响：钩子场景下「拒绝 → 模型看到理由」这条链路根本没被走到；生产路径（真实
tool call）不受影响（对照：permission 的真实 tool call 路径下，第二轮请求是
`['system','user','assistant','tool']`，工具结果正常在场）。

### 请求

1. 钩子在挂起前补发与生产一致的 timeline intent（至少
   `BlockOpened(kind=Tool, state=Prepared)`），让 `rounds` 不再为空；
2. 若希望覆盖「拒绝理由进模型上下文」，需要让钩子场景也产生成对的
   assistant `tool_calls`（或后端明确文档化：钩子只覆盖控制面，不覆盖上下文回灌）。

两条都不阻塞 TUI 现有门禁——§4 的账本判据已经能把拒绝路径钉住。

## 6. 证伪

新断言不是「怎么跑都绿」：

- 把 `permission-deny` 的按键从 `d` 改成 `a`（即实际批准）→
  `后端 timeline 记录否定终态` 立刻变红、`RESULT: FAIL`（已实测，随后还原）；
- `plan-reject` 的判据是**哈希指纹**：账本里必须是 `{"decision":"rejected"}` 的
  sha256，批准会写 `approved`，指纹对不上。

## 7. 门禁（本轮实跑）

```text
MODE=permission       RESULT: PASS   （肯定路径，未回归）
MODE=ask              RESULT: PASS
MODE=plan             RESULT: PASS
MODE=pager            RESULT: PASS
MODE=permission-hang  RESULT: PASS
MODE=ask-hang         RESULT: PASS
MODE=permission-deny  RESULT: PASS   （新增）
MODE=ask-dismiss      RESULT: PASS   （新增）
MODE=plan-reject      RESULT: PASS   （新增）
```

每个模式都断言：模态可见、按键已发、**后端否定终态**、无 cursor-position
timeout、无 panic、`exit=0`。

## 8. 复现与产物

```bash
for m in permission ask plan pager permission-hang ask-hang \
         permission-deny ask-dismiss plan-reject; do
  MODE=$m scripts/e2e-v2-interactions.sh
done
```

每个模式的隔离目录 `/tmp/qaqh-e2e-v2-$MODE/` 里留下：

- `tui.raw`：TUI 原始字节流（模态可见性断言用）；
- `timeline.json`：后端物化 timeline 快照；
- `provider-requests.json`：fake provider 收到的每一次请求体（否定路径诊断用）。
