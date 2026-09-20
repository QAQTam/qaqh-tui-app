# QAQH TUI v2 Inline 原型报告（V2-M1）

> 状态：**M1 原型完成；待 V2-M2 主题底座**
> 日期：2026-09-20
> 上游计划：[`2026-09-20-v2视觉与交互重构-plan.md`](../plan/2026-09-20-v2视觉与交互重构-plan.md)
> 关联规范：
> [`2026-09-20-v2终端提交协议-spec.md`](../spec/2026-09-20-v2终端提交协议-spec.md) ·
> [`2026-09-20-v2视觉token-spec.md`](../spec/2026-09-20-v2视觉token-spec.md)

---

## 0. 结论

V2-M1 原型验证通过：

- `Viewport::Inline` 可作为底部 live viewport；
- 已封口文本可通过 `Terminal::insert_before` 进入终端 scrollback；
- `CommitLedger` 能拒绝同一 `CommitId` 的重复提交；
- 默认 v1 全屏路径未改变，实验入口为 `--v2-inline`。

同时完成一次后端锚点批量适配：本地后端从 `bef097c2dd47` 前进到
`ffa6d84e838c`，`qaqh-client` 的 `Reconnecting` 新增 `ReconnectReason`；
TUI 已接入结构化重连原因，并覆盖 U-07 的 `lagged` 诊断文案。

---

## 1. 变更清单

### 新增

- `src/terminal/mod.rs`
- `src/terminal/commit.rs`
  - `CommitId`
  - `CommitDecision`
  - `CommitLedger`
  - `content_hash`
- `src/terminal/inline.rs`
  - `--v2-inline` 原型入口
  - inline viewport 绘制
  - Enter 提交 / `r` 重放 / `q` 退出

### 修改

- `src/main.rs`
  - 注册 `terminal` 模块；
  - 增加 `--v2-inline` 参数；
  - 帮助文本增加实验入口。
- `src/runtime.rs`
  - 适配 `ChannelStatus::Reconnecting { reason, .. }`；
  - 适配 `TimelineStatus::Reconnecting { reason, .. }`；
  - 新增 `reconnect_message()`，把 `Lagged` / `StreamTerminated` 转为可见诊断。
- `scripts/ci-linux.sh`
  - CI 后端 pin 从 `8c1c154` 升到 `ffa6d84e838c`，与 M1 适配锚点一致。
- `README.md`
  - 增加 V2 inline 原型运行说明。
- V2 计划与 parity matrix
  - 后端锚点更新为 `ffa6d84e838c`。

---

## 2. 原型行为

启动：

```bash
cargo run -- --v2-inline
```

行为：

| 输入 | 结果 |
|---|---|
| 字符 | 进入 live 输入 |
| Enter | 生成 `block-N`，通过 ledger 后 `insert_before` 到 scrollback |
| `r` | 用同一 `CommitId` 重放最近提交，应显示 duplicate |
| Backspace | 删除 live 输入 |
| `q` / Esc / Ctrl+C | 退出并恢复终端 |

原型不连接 daemon，不读取 token，不进入 alternate screen。

---

## 3. PTY 冒烟证据

执行：

```bash
cargo run --quiet -- --v2-inline
```

输入：

```text
hello v2
r
q
```

观察结果：

- scrollback 出现：

  ```text
    ❯ #0 hello v2
  ```

- 状态行显示：

  ```text
  committed block-0
  ```

- 按 `r` 后显示：

  ```text
  replay skipped · duplicate commit id
  ```

- 退出码 `0`，终端恢复。

这证明 M1 的核心不变式：

```text
同一 CommitId + 同一内容 = 最多提交一次
```

---

## 4. 后端锚点适配

| 项 | 原锚点 | 新锚点 |
|---|---|---|
| `qaqh-backend` | `bef097c2dd47` | `ffa6d84e838c` |
| `ReconnectReason` | 无 | `Lagged` / `StreamTerminated` |
| TUI 诊断 | 只有「断开」 | `lagged` / 稳定 code 可见 |
| U-07 | 未接线 | 代码侧已接通 |

适配原则：

- 只改边界层 `runtime.rs`，不扩散到 UI；
- 结构化原因直接来自 `qaqh-client`，不自行解析 wire；
- 新增锁 `reconnect_message_exposes_structured_reason` 防止文案回退。

---

## 5. 验证

```text
cargo fmt --check                            # 通过
cargo clippy --all-targets -- -D warnings    # 通过
cargo test --all-targets                     # 234 passed / 0 failed / 6 ignored
```

新增测试：

- `terminal::commit::tests::*` 4 条；
- `terminal::inline::tests::*` 3 条（含 inline viewport resize 后重绘）；
- `runtime::tests::reconnect_message_exposes_structured_reason` 1 条。

---

## 6. 已知边界

- 原型只提交纯文本，不接 timeline；
- resize 仅验证 inline viewport 的 resize 调用与重绘，不做 scrollback 清空重放；
- 不处理会话切换；
- 不处理主题 token；
- 不做多行 live block；
- 未捕获鼠标，符合 M1 预期。

这些分别进入 V2-M2 ~ V2-M5。

---

## 7. 下一步

V2-M2：主题底座。

- 建立 `src/theme/`；
- 实现 QAQH Night / Day / Terminal / Auto；
- 实现 truecolor / 256 / 16 / NO_COLOR 降级；
- 把 inline 原型改为消费 token；
- 增加主题快照测试。
