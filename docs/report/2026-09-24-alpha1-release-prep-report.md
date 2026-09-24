# alpha1 发布前准备报告（2026-09-24）

> 触发：距首个 alpha 发版 7 小时，「最小功能必须可用」。

## 0. 一句话

**alpha 不切 v2 协议**（plan §5.2 / issue #46 裁决：v2 是 **beta** 起的权威重连
协议，v1 是 2.0 兼容面），所以 alpha1 = 「V2 Agent View 默认 UI + Ringing v1 协议」
——这个状态本来就全绿。7 小时花在三件真正影响"可用"的事上：锚点跟进、把被默认
UI 切切换打哑的验证面修好、以及修掉默认 UI 的第一个动作 `Ctrl+N` 整条失效。

## 1. 锚点跟进：`a43a8bc` → `b77c251`

| 项 | 值 |
|---|---|
| 后端 tag | `tui-ringing-v2-interaction-causation-2026-09-24` |
| 后端 rev | `b77c2519f06f66c084bcb29d234e4a9c008777c5` |
| 对应 PR | `#324`（daemon 最小闭环）/ `#325`（control shape）/ `#326`（interaction 因果） |
| daemon sha256 | `2eb40ddbef52e8e052bf40a84b6347290eaef77596ef6f18486997243c17b13c` |

换锚点撞到的**唯一**破坏面：`ClientV2ControlState` 在 `#325` 由别名变成手写
结构体，`activity` / `tools` / `subagents` / `revision` / `last_fact_seq` 在 wire
上变成必填，而本仓测试 fixture 手写得省 → 红。改成用客户端自己的 `Default`
生成三个频道基线再覆写本用例关心的字段（后端再加必填字段不用回来补 fixture）。
`Cargo.lock` 无需刷新（两个 rev 之间 `Cargo.toml` 变化数为 0，`--locked` 通过）。

## 2. 三个老 harness 被默认 UI 打哑（已修）

`e2e-lease-expiry` / `e2e-restart` / `e2e-session-list` 仍在用 `script(1)` 驱动
PTY。v2 Agent View 初始化 inline viewport 时会发 `ESC[6n` 光标查询并等应答，
哑驱动无人应答 → TUI **初始化即 panic**：

```text
failed to initialize terminal: "The cursor position could not be read within a normal duration"
```

handoff §6 记过这个坑，但当时只修了 `smoke-tui.sh`。后果是 issue #7 里
"从未验证"的三项**连测都测不了**。修法：

- 抽出 `scripts/lib/pty-driver.py`（真 PTY + 应答 `ESC[6n` + 按时间表注入按键）；
- 抽出 `scripts/lib/vt_screen.py`：断言"屏幕上显示了什么"必须先重建屏幕 ——
  差分流里没变的空格根本不在字节流里，`Bun 引导 daemon` 会被去 ANSI 后拼成
  `Bun引导daemon` 而假红（实测）；
- 相位正则补 v2 的 `○ opening` / `● degraded`；
- lease-expiry / restart 的**相位判据**显式走 `--v1`：v2 Agent View 只在有活动
  会话时才画 `status_line`，而这两个 harness 刻意跑空 data root。

### 2.1 issue #7 三项的现状

| 项 | 结论 | 证据 |
|---|---|---|
| 租约过期自愈 | ✅ 已验证 | `e2e-lease-expiry` PASS：7 次 `/clients/open`、末相位 `● ready`、全程无 `✗ lost` |
| `Lagged` 终止帧恢复 | ✅ 已验证 | `e2e-v2-faults MODE=lagged` PASS（诊断文案可见 + 重连后会话仍可用） |
| 多标签 + 子代理并发 | ⏸ 需真实 LLM | 本机 env 无任何 provider 凭据；e2e 走 stub provider（`test-model`） |

## 3. 默认 UI 第一个动作整条失效（已修）

**现象**：空 data root 下按 `Ctrl+N`，屏幕毫无变化；再按一次 → **又建一个会话**
（实测连按两次 = 磁盘 2 个会话，界面全程不变）。

**定位**：

- daemon 侧完全正常 —— 命令收据 `state=succeeded`，journal 里
  `session_state_changed/created` 带**正确**的 `causation_id == command_id`；
- TUI 侧不开 tab、不 toast。`--v1` 也坏，只是首屏列表每 3s 自动刷新把
  「最近会话 — 1 个」显示出来，把故障遮住了；
- 根因：「新会话靠 `SessionStateEvent::Created` 那条可靠事件开 tab」在实践中
  不成立，而它是**唯一**的开 tab 路径。

**修法**（本仓，不动协议）：列表兜底 —— 有在途 create 时，把列表里 `created_at`
最新、本地还没 tab 的 seed 开出来并 toast；新建时立刻作废列表缓存；自动刷新在
`!pending_creates.is_empty()` 时**也**跑（此前只在 `tabs.is_empty()` 时跑，用户
已开着别的 tab 时新会话永远发现不了）；空态在途期间显示「正在创建会话…」。

**回归锁**：单测 `pending_create_opens_newest_session_from_list` + 反向闸
`list_refresh_without_pending_create_opens_nothing`；新增
`scripts/e2e-new-session.sh`（真 daemon + 真 PTY，断言 composer/`● ready`、
只建 1 个会话、出现创建文案）。**变异验证**：把三处 src 改动 stash 掉重跑 →
该 e2e 红（屏幕停在空态）。

## 4. 当前门禁状态（锚点 `b77c251`）

```text
cargo test --all-targets -- --test-threads=1      350 passed / 0 failed / 9 ignored
cargo clippy --all-targets -- -D warnings         PASS
cargo fmt -- --check                              PASS
scripts/static-gates.sh                           PASS（G1–G4）
scripts/perf-gate.sh                              PASS
scripts/tests/ci-linux-parse-test.sh              15 passed / 0 failed
scripts/smoke-tui.sh / TUI_ARGS=--v1              PASS
scripts/e2e-new-session.sh                        PASS
scripts/e2e-session-list.sh                       PASS
scripts/e2e-lease-expiry.sh                       PASS
scripts/e2e-restart.sh                            相位 ready(77b58956) → lost → ready(a7be7cb1)
e2e-v2-faults.sh 六模式 / e2e-v2-interactions.sh 九模式   PASS
```

## 5. 发布前仍待办

1. **合 PR #49 → main**（alpha 内容；CI 红是组织 CPU 配额，非代码）。
2. **打 tag / release notes / 回滚说明**（`--v1` 回退路径）。
3. **真模型冒烟**：本机 env 无 provider 凭据，e2e 全是 stub provider ——
   "真模型下能不能跑通一个完整回合"仍是 alpha **唯一没验证**的路径。
4. 明确写进 release notes 的**已知限制**：
   - v2 协议未启用（beta 才切）；
   - v2 Agent View 空态不显示连接相位（有活动会话才画 `status_line`）；
   - 多标签 + 子代理并发未验证（需真实 LLM）。
