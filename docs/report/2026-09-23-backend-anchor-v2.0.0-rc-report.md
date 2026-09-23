# 后端锚点升到 v2.0.0 RC 报告（TUI 侧消费）

> 日期：2026-09-23
> 触发：后端 issue 交接 **QAQ-Harness/qaqh-tui-app#44**（P1）
> 状态：已完成（锚点升级 + 断言升级 + 9 MODE 回归）

## 0. 结论

后端收口了 #43 指出的 `QAQH_TEST_PLAN_REVIEW` 保真度缺口（PR
`QAQ-Harness/qaqh-backend#301`），给出新锚点。TUI 侧完成三件事：

1. 锚点升到 **v2.0.0 RC**，daemon 重建、TUI 重编通过（**无 wire 破坏**）；
2. `MODE=plan-reject` 从「单一账本指纹」升级为 **三重断言**（timeline 物化 +
   provider 上下文配对 + 账本指纹），共 9 条子断言；
3. 9 个 MODE 全绿，仓库门禁全绿。

## 1. 锚点

| 项 | 值 |
|---|---|
| tag | `tui-anchor-2026-09-23-v2.0.0-rc`（annotated，tagger `QAQTam`，不可移动） |
| commit | `1e78b7ce98df2875f036dc8aec1e2371779d0f58` |
| 上一版 | `tui-anchor-2026-09-23-p3` @ `8dbe22e` |
| 中间版 | `tui-anchor-2026-09-23-p3-typed-tools` @ `84e08117`（= #301 的 merge commit） |

已核对：`84e08117` 是 `1e78b7ce` 的**祖先**（`git merge-base --is-ancestor` 通过），
即新锚点包含 #43 的修复。

### 落地动作

```bash
# 锚点 worktree 切到新 rev（detached）
git -C ../qaqh-backend-anchor checkout --detach 1e78b7ce98df2875f036dc8aec1e2371779d0f58
cargo build -p qaqh-daemon --bin qaqh-daemon     # 22.5s
sha256sum ../qaqh-backend-anchor/target/debug/qaqh-daemon
#   c4c4f2e63cbbbd37958cc46cb8b42e5cb5ad1c57d3682cfb50eac1000ae33432

# 本仓
scripts/ci-linux.sh  QAQH_BACKEND_REV → 1e78b7ce98df2875f036dc8aec1e2371779d0f58
.cargo/config.toml   注释同步（该文件不入库）
cargo build --bin qaqh-tui                       # 14.2s
```

## 2. `MODE=plan-reject` 断言升级（issue #44 §2）

### 升级前后

| | 升级前 | 升级后 |
|---|---|---|
| 判据 | 只有会话事实账本 `decision_ref("rejected")` | timeline ×5 + provider 上下文 ×3 + 账本 ×1 |
| 能发现 | 裁决没送达后端 | 裁决没送达 **/** 转写没物化 **/** 理由没回灌模型 |

### 断言清单（实测输出）

```text
[✓] 后端证据 · timeline：存在 tool:test-plan-review-* 块
[✓] 后端证据 · timeline：tool.name == plan_submit
[✓] 后端证据 · timeline：tool.state == failed
[✓] 后端证据 · timeline：tool.output 含 `Plan rejected: <理由>`
[✓] 后端证据 · timeline：所属回合 rounds 非空
[✓] 后端证据 · provider：assistant tool_calls 含 test-plan-review-*
[✓] 后端证据 · provider：存在同 id 的 role=tool 消息
[✓] 后端证据 · provider：tool 内容含拒绝理由（理由回灌模型上下文）
[✓] 后端证据 · 账本：decision=rejected 指纹（第三重）
```

### 升级前 vs 升级后实测到的后端行为变化

同一模式在**旧锚点**（`8dbe22e`）与新锚点下的快照对照：

```jsonc
// 旧锚点：rounds 空，块不存在
{"turn_id": "t1", "state": "completed", "rounds": []}

// 新锚点：块物化 + 拒绝结果落在块上
{"turn_id": "t1", "state": "completed", "rounds": 2,
 "blocks": [{"block_id": "tool:test-plan-review-t1", "kind": "tool",
             "tool": {"name": "plan_submit", "state": "failed",
                      "output": "Plan rejected: e2e-plan-reject-marker\n\nTUI contract test plan…"}}]}
```

provider 上下文同样从「只有 system+user」变成配对齐全：

```text
旧：request 0: ['system', 'user']
新：request 0: ['system', 'user', 'assistant', 'tool']
     assistant.tool_calls[0].id = test-plan-review-t1, fn = plan_submit
     tool.tool_call_id          = test-plan-review-t1
     tool.content               = <qaqh_tool_result status="error">…Plan rejected: e2e-plan-reject-marker…
```

## 3. 实现细节：一个自己踩的坑

首版把 `timeline_tools()` 的返回当成 `(turn, block)` 用，但它返回的是
`(turn, tool)`，于是 `block.get("block_id")` 恒为 `None` → 5 条 timeline 断言
全红（`n=0`），而 provider 与账本断言正常。**这正是「多重断言」的价值**：
单一账本判据当时是绿的，会把这个 bug 放过去。已修正并复跑。

## 4. 门禁

```text
cargo fmt --check                                 OK
cargo clippy --all-targets -- -D warnings         OK
cargo test --all-targets                          337 passed / 0 failed / 8 ignored

MODE=permission        PASS    MODE=permission-hang   PASS
MODE=ask               PASS    MODE=ask-hang          PASS
MODE=plan              PASS    MODE=permission-deny   PASS
MODE=pager             PASS    MODE=ask-dismiss       PASS
                               MODE=plan-reject       PASS（9 条后端证据）
```

## 5. 仍然阻塞的：#42 在新锚点下**依旧复现**

四个故障钩子（`SSE_TERMINATE` / `TIMELINE_GAP` / `COMMAND_ACK` /
`SESSION_404_SEED`）的接线前提是「无注入基线通过」。新锚点下实测仍红：

```text
MODE=none bash scripts/e2e-v2-faults.sh
  [✓] 基线：用户消息可见
  [✗] 基线：回复可见
RESULT: FAIL
```

`#42` 仍是 open 且无评论。四个钩子继续暂停接线。

## 6. issue 状态

- **#43**：后端已 close（completed），无需 TUI 侧再关；
- **#44**：本报告即其验收产物，已在 issue 下附命令与结果。
