# 后端锚点刷新报告（2026-09-20）

> 状态：**已完成**
> 日期：2026-09-20
> 关联计划：[`2026-09-20-v2视觉与交互重构-plan.md`](../plan/2026-09-20-v2视觉与交互重构-plan.md)

---

## 0. 结论

TUI 的后端锚点从：

```text
ffa6d84e838ced425d1b12b2ae896c824d512c56
```

刷新到当前验证点：

```text
50d3dc1dcfefeb6af4c575deeb049fe5d74a079e
qaqh-backend @ betav2
```

刷新后 TUI 全门禁通过，不需要修改 Rust 代码。

---

## 1. 刷新原因

后端 `betav2` 已连续合入 recovery intent、replay window 与相关文档：

```text
50d3dc1 Merge PR #175: replay window status
38977f7 docs(v2): 回写 replay window 状态 (Closes #174)
80e99dd Merge PR #173: replay window persistence
c6aada7 feat(session): 实现 replay window 持久化 (Closes #172)
cee150b Merge PR #171: recovery intent status
efc8ec3 docs(v2): 回写 recovery intent 状态 (Closes #170)
```

本次显式验证该点，避免 CI 继续钉在旧锚点造成无意义的漂移。

---

## 2. 兼容性判断

从旧锚点到新锚点的变更集中在：

```text
crates/qaqh-session/
docs/
```

TUI 依赖树包含：

```text
qaqh-client
qaqh-config-api
qaqh-domain
qaqh-ringing
qaqh-types
```

不包含 `qaqh-session`。因此新合入的 recovery intent / replay window 持久化
不会进入 TUI 编译或运行路径。

结论：**契约面无 TUI 适配项；无需修改 Rust 代码。**

---

## 3. 验证结果

在 `qaqh-backend @ 50d3dc1dcfef` 上执行：

```text
cargo fmt --check                            passed
cargo check --all-targets                    passed
cargo clippy --all-targets -- -D warnings    passed
cargo test --all-targets                     274 passed / 0 failed / 6 ignored
scripts/tests/ci-linux-parse-test.sh         15 passed / 0 failed
```

---

## 4. 固化的文件

- `scripts/ci-linux.sh`
  - `QAQH_BACKEND_REV` → `50d3dc1dcfefeb6af4c575deeb049fe5d74a079e`
- `docs/plan/2026-09-20-v2视觉与交互重构-plan.md`
  - 后端锚点与 D6 决策同步刷新
- `docs/spec/2026-09-20-v2-v1-parity-matrix.md`
  - D-07 锚点同步刷新

历史报告仍保留当时的锚点，不篡改历史记录。
