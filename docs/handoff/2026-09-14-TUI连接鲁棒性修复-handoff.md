# TUI 连接鲁棒性修复交接（2026-09-14）

## 交接摘要

修复「流式中断后 401 卡死、无法回连」的 P0 缺陷。根因：TUI 自建传输层把 daemon renew 的 plain 401 误判为致命错误，supervisor 直接 `return` 终止连接生命周期。

- 改动文件：`src/transport/http.rs`、`src/runtime.rs`（仅这两个）
- 新增回归测试：14 个（http.rs 7 + runtime.rs 7）
- 验证：`cargo clippy --all-targets` 零 warning；`cargo test` 151 passed / 1 failed（失败为既存 Windows 问题，非本次引入）
- 关联文档：`docs/report/2026-09-14-TUI连接生命周期401卡死-report.md`、`docs/plan/2026-09-14-TUI传输层迁移qaqh-client-plan.md`

## 改了什么

### 1. 401 三态分类（`src/transport/http.rs`）

daemon 对三种失败都回 401，其中两种 body 是 plain text：

| body | 含义 | 修前分类 | 修后分类 |
|---|---|---|---|
| `{"code":"lease_required",...}` | 租约缺失/过期/seed 未 attach | `LeaseRequired` | 不变 |
| `unauthorized` | Bearer token 被拒（`auth.rs:12`） | `Unauthorized` | 不变 |
| `lease expired or unknown` | renew 时租约已死（`command.rs:137`） | ❌ `Unauthorized`（**误判**） | ✅ `LeaseRequired` |

- `http.rs:32` 新增 `PLAIN_UNAUTHORIZED` 常量
- `http.rs:174-184` `classify` 按 body 内容区分三态
- `http.rs:63` `is_fatal()`：**仅**协议代差
- `http.rs:68` `is_credential()`：token/租约类
- 删除易误用的 `is_unauthorized()` / `is_lease_required()` / `is_unsupported_version()`

### 2. 凭据热更新（`src/transport/http.rs`）

- `base_url` / `token` 由 `String` 改为 `RwLock<String>`
- `http.rs:110` `apply_discovery(base_url, token) -> bool`：原地换值，返回是否变化
- `http.rs:144` `fn token()`：clone 出锁，避免 `RwLockReadGuard` 跨 `await` 使 future `!Send`

### 3. supervisor 自愈（`src/runtime.rs`）

- `runtime.rs:162` `SupervisorAction{Stop,Reconnect,Retry}` + `runtime.rs:176` `supervisor_action()`
  —— **纯函数，是本次修复的回归锁**：只有 `is_fatal()` 能产出 `Stop`
- `runtime.rs:193` `refresh_credentials()`：重读 `daemon.json` 热更新凭据
- renew 失败：凭据类 → `refresh_credentials` + `break` 重新 open（**不再 `return`**）
- open 失败：仅 `is_fatal()` 才 `return`；凭据类先刷新再退避重试
- supervisor 启动即对齐 discovery（daemon 可能在 TUI 运行期间重启过）

### 4. 流感知重协商（`src/runtime.rs`）

- 两条流各记录 `known_generation`（`runtime.rs:408`、`:579`）
- `conn_rx.changed()` 同时比较 epoch 与 generation：generation 变化即重连（同 epoch 内保留 cursor 续传）

### 5. 终止帧归一（`src/runtime.rs`）

- `runtime.rs:316` `STREAM_TERMINATED` + `:319` `stream_terminated_code()`
- 频道流与 timeline 流在 `reset_required` **之前**拦截，上报含 `code` 的 `StreamIssue` 并重连/重定基

## 验证命令

```bash
cd D:/project/qaqh-tui-app

# 编译与静态检查
cargo check --all-targets          # 期望：零 error
cargo clippy --all-targets         # 期望：零 warning

# 本次新增回归测试（14 个）
cargo test transport::http         # 期望：7 passed
cargo test runtime::               # 期望：7 passed

# 全量
cargo test                         # 期望：151 passed / 1 failed
# 已知失败：app::slash::tests::absolute_path
#   原因：Windows 上 "/tmp/foo" 不被判为绝对路径（平台差异，非本次引入）
#   证据：git stash 本次改动后该测试在干净树上同样失败
```

## 人工验证（建议）

1. **daemon 重启自愈**：TUI 运行中重启 daemon（换 token/端口）→ 期望 TUI 显示「已重读 daemon 记录，重新协商」并自动恢复，无需重启 TUI。
2. **租约过期自愈**：流式输出期间人为让 daemon 停顿超过 30s TTL → 期望 TUI 自动重新 open，不出现永久 `✗ lost`。
3. **缓冲溢出**：制造事件风暴触发 `Lagged` → 期望 TUI 显示「服务端终止流（lagged），重连重定基」并恢复。

## 已知遗留（不在本次范围）

| 项 | 位置 | 说明 |
|---|---|---|
| BOM 剥离缺失 | `src/transport/sse.rs` | 中间层注入 BOM 时首帧丢；`qaqh-client` 已修（BUG-2026-09-13-17），建议随迁移解决 |
| 无手动重连入口 | `src/ui/status_bar.rs:28` | `Lost` 相位只显示错误摘要；建议加按键触发重新协商 |
| 未迁移 `qaqh-client` | `Cargo.toml:12` | 见迁移评估 plan |
| 后端 HEAD 编译失败 | `qaqh-backend` `crates/qaqh-runtime/src/ringing/hub.rs:308` | `rebase/76` 11 个 error，阻塞后端本地验证（**与本仓无关，但会影响联调**） |

## 后端侧待办（供后端同事）

`qaqh-backend` 当前 HEAD（`rebase/76` @ `c1e5a3d`）编译失败：

```
$ cargo check -p qaqh-runtime --lib
error[E0308] ringing/hub.rs:1018  live.entry(channel) 期望 (RingingChannel, String)
error[E0599] ringing/hub.rs:1499  no method `seed_keys`
error[E0599] ringing/hub.rs:1500  no method `slot_if_present`
error[E0277] ringing/hub.rs:742/808  SeedChannelState: Clone 未满足
... 共 11 个 error
```

`hub.rs:308` 的 `channels` 字段是 `HashMap<RingingChannel, HashMap<String, SeedChannelState>>`，但 `ChannelShards` 结构体（`:207`）与 `shard.slot_if_present` / `seed_keys` 调用点仍是 PR 分支 `833769c` 的形态。与 PR 分支 diff 达 +571/−144，疑似 rebase 冲突解错。`subscribe()`（`:1018`）漏了 `(channel, seed)` 元组。
