# 故障钩子接线阻塞：timeline 流 seq 缺口导致 v2 transcript 永不渲染（2026-09-23）

> 状态：**已定位、待后端确认根因**（跨仓）
> 发现于：为后端 `issue #41` 的「四个故障钩子」接线做 harness 时
> 锚点：`tui-anchor-2026-09-23-p3` → `qaqh-backend @ 8dbe22e`
> 关联：[契约测试钩子 spec](../../../qaqh-backend/docs/spec/2026-09-23-TUI契约测试钩子-spec.md)

## 0. 一句话

**在没有任何故障注入的情况下**（`MODE=none`），一个新会话跑一个短回合后，
timeline SSE 的 live 序列**从 seq 3 开始**（1、2 从未下发），客户端严格
`cursor + 1` 判据必然报 gap；gap 恢复拿到的又是**回合中途**的快照（未封口），
于是 v2 的提交管线永远拿不到 sealed 块 → **transcript 全空**。

## 1. 复现

```bash
# 无任何 QAQH_TEST_* 注入
MODE=none bash scripts/e2e-v2-faults.sh
# → [✗] 基线：回复可见    （用户消息与回复都不进 scrollback）
```

实测输出：

```text
daemon pid=… mode=none faults={}
created seed=…
tui exit=0 bytes=11199 cursor_queries=3 provider_requests=1
  [✓] 基线：用户消息可见      ← 命中的是 composer 里的输入，不是 transcript
  [✗] 基线：回复可见
  RESULT: FAIL
```

## 2. 证据链

### 2.1 客户端日志（`QAQH_TUI_LOG`，本次为此新增的开关）

```text
[INFO] [qaqh-client] timeline 91f59df9 gap recovered at cursor 3
[WARN] [qaqh-client] timeline 91f59df9 reconnect in 1000ms: timeline SSE gap: expected seq 1, received 3
```

即：客户端 cursor=0（新会话），第一个收到的 entry 是 **seq 3** → 判 gap。

### 2.2 裸 SSE 客户端实测（`/tmp/probe_seq2.py`）

**先订阅** timeline SSE、**再**跑一个回合，收到的是：

```text
订阅已建立（此时收到 0 帧）
=== live 序列（共 7 帧）===
  <epoch>:timeline:3
  <epoch>:timeline:4
  …
  <epoch>:timeline:9
```

**seq 1、2 从未下发**——不是客户端丢帧，是服务端 live 序列本身跳号。

### 2.3 快照对照

```text
空会话      watermark=0  turns=0
回合后      watermark=9  turns=1   （turn state=completed sealed=True，1 个 block）
```

9 个 seq 里只有 1 个是可投递的 block——说明 seq 会为「非投递项」消耗，
但 **SSE 侧的连续性判据是 `cursor + 1`**，两者对不上。

### 2.4 v2 提交管线为何最终什么都不出

加了临时 trace（已还原）后：

```text
[TRACE] rebaseline seed=… turns=0          ← activate 的初始快照
[TRACE] sync seed=… turns=0 version=1 pending=0
…
[TRACE] sync seed=… turns=1 version=2 pending=0   ← 恢复用的快照到了，但产不出提交
```

`sync_turns` 只对 `BlockState::Sealed` 产生候选；gap 恢复时回合还在跑，
快照里是 Live 块 → 候选为空。而后续「封口」事件又因为流处于 gap 重连循环
永远收不到 → **该回合永远不会被提交到 scrollback**。

## 3. 影响面（比表面更大）

- 这不是钩子的问题，**与四个 `QAQH_TEST_*` 注入无关**；
- `scripts/e2e-v2-interactions.sh` 的 `permission` / `ask` / `plan` / `pager`
  四个模式此前「PASS」是因为**只断言 modal / 按键 / 无 panic**，
  **从未断言 transcript 内容**——所以这个缺陷一直不可见；
- 短回合（fake provider 秒回）100% 触发；真机长回合可能因后续事件补齐而不明显。

## 4. 待确认的根因（需要后端）

已排除：

- daemon 未授权：`session_resume` 后订阅 timeline 返回 **200**（裸探针验证）；
- 客户端未订阅：客户端日志显示流已建立并在收帧；
- gap 恢复本身：`recover_gap` 正常返回并推进 cursor。

**待查**：为什么 live 序列从 3 开始。候选方向（daemon 侧）：

1. seq 1、2 被 hub 的 `subscribe_timeline()` / `timeline_replay_since()` 窗口
   漏掉——订阅与 replay 之间、或 replay 与 live 之间的边界处理；
2. seq 1、2 属于**不可投递**的 entry（如 resume 期间的内部事件），但它们
   仍然占用了 `next_seq`，使「可投递序列」天然不连续；
3. `should_deliver_timeline_live` 的 `leases.owns_seed` 在会话刚 resume 时
   尚未生效，把最前面两帧判成「非本会话」而丢弃。

> 后端 handoff 里写过「客户端模型级回归，cosplay `expected == cursor + 1`
> 判定并断言不重复下发」——那条回归可能没覆盖「新会话 + 首个回合」这条路径。

## 5. TUI 侧已做（本次）

- **新增 `QAQH_TUI_LOG=<path>`**（`src/main.rs` 的极简文件 logger + `log` 依赖）。
  TUI 此前**完全没有 logger**，`qaqh-client` 的诊断全被丢掉——真机排查只剩
  UI 上那句没有原因的「timeline[….] 断开，1000ms 后重连」。这个缺口直接卡住了
  本次定位，值得长期保留。
- **新增 `scripts/e2e-v2-faults.sh`**：四个故障钩子的 harness
  （`lagged` / `gap` / `ack-delay` / `ack-hang` / `session-404`，外加 `none` 基线）。
  **当前预期 FAIL**——失败本身就是本缺陷的证据。
- 临时 trace（`QAQH_TUI_TRACE`）已全部还原。

## 6. 建议

1. 后端确认 §4 的根因，并给出「新会话首个回合」的 timeline seq 连续性回归；
2. 修好之前，**四个故障钩子的接线先不做**——它们的断言全都依赖
   transcript 内容，现在接了也只能红；
3. 顺带建议：`scripts/e2e-v2-interactions.sh` 四个模式的断言补一条
   「assistant 回复出现在 scrollback」，否则同类缺陷还会被漏掉。
