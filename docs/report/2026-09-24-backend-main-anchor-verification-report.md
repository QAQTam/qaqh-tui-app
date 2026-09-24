# 后端 Ringing v2 最小锚点验证报告（2026-09-24）

## 0. 一句话

后端 Ringing v2 最小锚点 `tui-ringing-v2-types-2026-09-24@a43a8bc` 已通过 TUI
真实 daemon 的故障套件六模式、interaction 套件九模式、默认 Agent View smoke
与 `--v1` 回退 smoke；`session-title` panic 未复现。该 tag 已切换为 TUI CI 与
本机 anchor。

## 1. 验证锚点

| 项 | 值 |
|---|---|
| 后端 rev | `a43a8bcfb01e4cf0f97154f942d874f60cd54aa6` |
| 后端 tag | `tui-ringing-v2-types-2026-09-24` |
| 后端 PR | `#322`（已合并） |
| TUI rev | `main@9c12f4d` 加本轮默认 UI/reducer 改动 |
| daemon 构建 | `cargo build -p qaqh-daemon --bin qaqh-daemon` |
| daemon sha256 | `40de1873b92b0403682fa60b10ad6e934f9be23fece7e802087e8aff971d9f9b` |
| 锚点 worktree | `/home/qaqtamsy/项目/qaqh-backend-anchor`（detached `a43a8bc`） |

`a43a8bc` 是在 `e0753ba` 的 #314 / #315 修复之上增加 Ringing v2 最小类型与
`qaqh-client` typed API；本轮最终验证均以 `a43a8bc` 为准。

## 2. 故障套件：6/6 PASS

命令：

```bash
for mode in none lagged gap ack-delay ack-hang session-404; do
  MODE="$mode" bash scripts/e2e-v2-faults.sh
done
```

结果：

| MODE | 关键判据 | 结果 |
|---|---|---|
| `none` | 用户消息与回复可见 | PASS |
| `lagged` | lagged 诊断 + 重连后回复可见 | PASS |
| `gap` | re-baseline 后用户消息与回复都还在 | PASS |
| `ack-delay` | 慢 ack 不阻塞回合完成 | PASS |
| `ack-hang` | 命令挂起时 UI 不卡死、Ctrl+Q 干净退出 | PASS |
| `session-404` | 404 可见且非子会话不被关闭 | PASS |

对应 issue：

- `qaqh-backend#314`：新会话 seq 空洞与 seal 裁剪修复。
- `qaqh-tui-app#42`：TUI 侧复验。

## 3. Interaction 套件：9/9 PASS

命令：

```bash
for mode in permission ask plan pager permission-hang ask-hang \
            permission-deny ask-dismiss plan-reject; do
  MODE="$mode" bash scripts/e2e-v2-interactions.sh
done
```

结果：

- `permission` / `ask` / `plan`：真实 daemon + fake provider + PTY 通过。
- `pager`：`$PAGER` 挂起/恢复通过。
- `permission-hang` / `ask-hang`：应答超时、daemon 回收后干净退出通过。
- `permission-deny`：timeline `failed + [DENIED]` 权威证据通过。
- `ask-dismiss`：timeline `cancelled` + 账本 `dismissed` 通过。
- `plan-reject`：timeline 块 + provider 上下文配对 + 账本指纹三重证据通过。

## 4. `session-title` / #315

收集本轮 15 个 daemon 日志后，未出现：

```text
thread 'session-title' ... panicked
```

也未出现任何 `panicked`。这证明 #315 的“无 Tokio runtime 线程里 reqwest
`send()` 先求值”崩溃已不再复现。

**未证明项（如实记录）**：本轮 fake provider 对所有请求都返回
`text/event-stream`，而标题总结走的是同步非流式调用；因此不能把“标题最终被
LLM 覆盖”当作已证明。当前能确认的是：

- 同步标题路径不再 panic；
- fallback 标题已落盘（例如 `run fault test` / `run permission test`）；
- 要证明 LLM 覆盖，需要给 fake provider 增加一个识别标题请求并返回普通 JSON
  的分支，或改用真实 provider。

## 5. 结论

- `a43a8bc` 的 #314 / #315 修复与 Ringing v2 最小类型在 TUI 真实链路成立。
- TUI CI pin 已更新到 `a43a8bc`，本机 anchor worktree 已切到同一 tag。
- 故障套件头部已更新为“旧 b40 预期红、当前 a43 全绿”的口径。
- `e2e-v2-faults.sh` 不再需要“带修复版 daemon”手工覆盖路径。
- 后续 daemon `/ringing/v2` 原子 subscribe/replay 属于 P0-3，不在本报告范围。
