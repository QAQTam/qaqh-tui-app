# qaqh-tui-app P 级漏洞/错误审计报告

- 日期：2026-09-19
- 方法：codegraph（无索引，已尝试；按规范改用 Read/Grep 手工建图）+ rust skills（rust-router / coding-guidelines / unsafe-checker / m06-error-handling / m07-concurrency / m15-anti-pattern / domain-cli）
- 约束：未翻看 `docs/`，仅分析 `src/` + `Cargo.toml` + `cargo check`
- 判定标准：P0=UB/崩溃/注入/死锁/数据竞争；P1=生产可达 panic/静默丢数据/ poison 致崩；P2=健壮性；P3=测试代码/风格

## 结论

- P0：0 个
- P1：0 个（生产可达的 panic/越界/注入均已排除，见下）
- `cargo check`：通过（`Finished dev profile`，EXIT 0）

## 逐项核查

### 1. unsafe（P0 排查）

- 旧 `src/app/render/bench.rs` 已随 v1/inline 清理删除；当前 `src/` 无 unsafe。
- 缺 `// SAFETY:` 注释，按 unsafe-checker 记 P3，不构成漏洞。

### 2. 并发/锁（m07）

- `src/runtime.rs:240,267,329,331,346,432,444`：`std::sync::{RwLock,Mutex}.lock().expect()`。
- 临界区仅 `clone/set/Instant`，无跨 `.await` 持有（`set_tracked_seeds:266-273` 先放锁再 `spawn`；`rebuild_inner` 中 `sleep` 时无锁）。
- 仅锁毒化时 panic，需先有其它 panic，记 P2。
- 建议：改为 `unwrap_or_else(|e| e.into_inner())`。

### 3. wire 事件处理（m06/m15）

- `src/app/timeline_model.rs:536` `E::TurnOpened => unreachable!()`。
- 外层 `src/app/timeline_model.rs:470-516` 已拦截全部 `TurnOpened` 并 `return`，当前死代码、不可达。
- 按 m06/m15 外部输入不应 `unreachable`，记 P2。
- 建议：改为 `return None` + 计数，与 `dropped_missing_turn` 同口径。

### 4. 交互面板索引（P1 排查，已排除）

- `src/app/interaction.rs:226`：`focus=min(len.saturating_sub(1))` 箝位。
- `src/app/interaction.rs:282,291`：经 `questions.get(focus)` 校验后才索引 `selections[focus]/customs[focus]`。
- `src/app/interaction.rs:312-316` `EditCommit` 仅当 `editing_custom.is_some()` 可达，而 `editing_custom` 仅由校验过的 `StartEdit` 置位；空 `questions` 时无法进入编辑态。
- `src/app/session.rs:79-80`：`AskPanel::new` 保证三者同长原子构造；`src/app/mod.rs:1032` 整体替换，不存在半更新。
- `src/ui/v2/modal.rs` 渲染侧同不变量。
- 结论：无生产可达越界。

### 5. render panic（已排除）

- 旧 `src/app/render/mod.rs` 已随 v1/inline 清理删除；当前渲染路径无生产可达 panic。

### 6. pager 命令执行（注入排查，已排除）

- `src/terminal/agent/mod.rs` + `src/app/pager.rs`：`sh -c` 执行 `$PAGER`，`tmp` 路径经 `shell_quote` 单引号转义。
- `$PAGER` 仅取本地 env，属用户自控，非远端输入。`tmp` 名含 `pid`，本地 TOCTOU 风险低。
- 结论：非 P 级注入。

### 7. 静默丢弃（m06 排查，已排除）

- `src/app/session_ops.rs`：`.ok()` 逐项跳过畸形条目，但顶层非数组仍经 `ok_or_else` 回传 `Err`，注释写明 G2 宽松契约。
- `src/app/mod.rs:1667` `spawn_api` 用 `client_opt()` 非 `client()`，断连时任务照起、结果走 `Err`，无 panic。
- 结论：设计如此，非 P 级。

## 复现/验证

```bash
cargo check  # 通过
```

`rg -n '\.unwrap\(\)|\.expect\(|panic!|unreachable!' src/ --glob '*.rs'` 的命中已按生产/测试、在位/死代码逐条分类，见上。
