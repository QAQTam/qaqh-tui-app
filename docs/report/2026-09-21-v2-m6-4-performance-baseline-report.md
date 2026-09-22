# QAQH TUI v2 M6.4 性能基线报告（增量）

> 日期：2026-09-21
> 状态：进行中

## 0. 结论

M6.4 已建立第一条 v2 commit runtime 基准，并据此发现并修复了 Agent View
每帧重扫整条 timeline 的性能问题。

基准场景：

```text
110 turns × 20 tools
≈ 2533 个 V2 blocks
```

优化前，无新增内容的一次同步仍需约 **27.7 ms**。加入 `TimelineModel.version`
版本门后，同一场景降为 **1–3 µs**；live delta 从整历史重扫降到约
**8–28 µs/帧**。绝对耗时会随 CPU powersave/boost 状态波动，内存读数更稳定。

## 1. 复现命令

```bash
cargo test render::bench::bench_v2_commit_runtime -- --ignored --nocapture
```

## 2. 基准结果

优化后：

```text
=== V2 commit runtime（110 turns × 20 tools）===
replay: 2533 blocks / 85.72 ms
runtime live 5650 KB / peak 6181 KB
高水位增量: 0 blocks / 3 µs
live delta ×20: avg 28 µs/帧
render: 24432 lines / 529.40 ms
first frame: v1 cache 31.44 ms | v2 lazy replay+chunk 6.55 ms | \
v2 full replay+chunk 48.48 ms
```

关键读数：

| 指标 | 结果 | 说明 |
|---|---:|---|
| replay | 85.72 ms / 2533 blocks | 清屏后的完整 projector + ledger + commit 计划 |
| 无新增同步 | 3 µs | `TimelineModel.version` 未变时零扫描 |
| live delta | 28 µs/帧 | 只从最后一个已知 turn 开始增量同步 |
| 首帧 | v1 31.44 ms / v2 6.55 ms | v2 每帧 replay 8 turns + 提交首个 32-block chunk |
| runtime 常驻 | 5650 KB | projector seen 状态 + commit ledger + pending |
| runtime 峰值 | 6181 KB | replay 临时分配峰值 |
| render | 163.52 ms / 24432 lines | 2533 blocks 的完整 V2 render，非每帧路径 |

## 3. 性能修复

### 问题

`AgentState::sync` 原先每帧调用 `V2TranscriptRuntime::sync_timeline`。即使
timeline 没有变化，也会遍历当前窗口并让 projector 重新检查所有 turn/block。
110 回合场景下，这个空转约 27.7 ms/帧，直接吃掉实时输入预算。

### 修复

`TimelineModel` 已有单调 `version`，每次可见模型变化都会递增。新增：

```text
V2TranscriptRuntime::sync_timeline_versioned(seed, turns, version)
V2TranscriptRuntime::replay_from_scratch_versioned(seed, turns, version)
```

规则：

- version 未变：直接返回空计划，不扫描 timeline；
- version 变化：走既有高水位过滤和增量 projector；
- seed 切换：重置 version，并完整 replay；
- unversioned `sync_timeline` 保留给测试与显式全量调用。

同步起点从“第一个已知 turn”改为“最后一个已知 turn”。旧回合晚封口不能被追加
到已经写出的新历史之后，否则会破坏 append-only 顺序；跳过它既符合 scrollback
顺序，也把每个 delta 的扫描窗口缩到活动 turn。

### 首帧分块提交

首次 replay 原先把全部 pending 在一个 `commit_pending` 调用里渲染并写入
scrollback；110 回合场景首帧约 635 ms。现在每帧最多 replay 8 个 turn，
并最多提交 `COMMIT_CHUNK_BLOCKS = 32`，剩余内容留在 AgentState 队列后续帧继续写：

- live viewport 先出现；
- 历史 scrollback 渐进补齐；
- 顺序仍由同一个 pending 队列保持；
- 会话切换会清空旧 seed 的待提交队列。

当前同机对照：

```text
v1 cache 31.44 ms
v2 lazy replay + first chunk 6.55 ms
```

首帧 R-01 已达标。

回归锁：

```text
ui::v2::runtime::tests::versioned_sync_uses_timeline_version_gate
terminal::agent::tests::first_snapshot_replays_once_then_syncs_incrementally
```

## 4. 规模曲线

```bash
cargo test app::render::bench::bench_v2_runtime_scale_curve -- --exact --ignored --nocapture
```

```text
turns   blocks    replay ms live delta µs       render ms  steady live
   25      578         6.79             7           39.07        37 KB
   50     1153        11.28            19           76.17        70 KB
  110     2533        26.26             8          162.08       138 KB
  220     5063        51.81            10          321.40       274 KB
  440    10123       103.66            12          643.25       545 KB
```

结论：

- replay、render、runtime steady live 均近似线性；
- live delta 在 25→440 回合间保持 7–19 µs/帧；
- 440 回合完整 replay 约 104 ms，不进入逐帧路径。

v1 对照（`bench_long_context_memory_curve`）：

```text
440 turns: model 20000 KB + evicted render cache 2297 KB
```

v2 440 回合 steady runtime 为 545 KB，显著低于 v1 淘汰后 render cache 的
2297 KB。R-03“长会话 UI 驻留显著下降”达标。

内存修复由三部分组成：

- ledger 改为 seed 分层的 128-bit 身份指纹，不再为每个 block 保存完整
  seed/turn/block 字符串；
- 删除生产未使用的 `order` 副本；
- 完整 replay 后对超大 `VecDeque` 做 `shrink_to_fit`，释放已 drain 的槽位容量。

## 5. 长会话 replay 截断修复

曲线最初在 220/440 回合停在 4096 blocks。根因是完整 replay 复用了增量提交
队列的有界 `enqueue`，超过 `DEFAULT_PENDING_CAP = 4096` 后从头部静默丢弃。

修复：

- 增量路径继续使用有界 `enqueue`，防止异常事件率撑爆内存；
- 会话切换/resize 的完整 replay 改用 `enqueue_replay`，不裁剪；
- 新增回归锁：

```text
terminal::transcript::tests::replay_enqueue_does_not_drop_past_capacity
ui::v2::runtime::tests::full_replay_is_not_truncated_by_incremental_queue_cap
```

## 6. PTY 回归

优化后重新跑通：

```text
scripts/e2e-v2-session-switch.sh     PASS
scripts/e2e-v2-resize-stress.sh      PASS
scripts/e2e-v2-reconnect.sh          PASS
```

因此版本门没有破坏增量更新、分页、会话切换、resize 或重连。

## 7. 全量门禁

```text
cargo test --all-targets                          331 passed / 0 failed / 8 ignored
cargo clippy --all-targets -- -D warnings         通过
cargo fmt --check                                 通过
```

## 8. 未完成

- 尚未建立首帧延迟、长会话 UI 驻留和 resize replay 的定量 v1/v2 对照；
- render 基准包含全量 24432 行，尚未拆分 live viewport 与 scrollback commit；
- v2 runtime steady memory 已低于 v1 淘汰后的 render cache；
- 默认切换仍属 M7；显式 `--v1` 回退已就绪。

## 9. Feature Flag / 回退

启动模式优先级已冻结：

```text
--v1
  > --v2-inline / QAQH_V2_INLINE
  > --v2-agent / QAQH_V2_AGENT
  > 默认 v1
```

`--v1` 是显式回退闸，压过环境变量，保证单次启动可回到 v1。新增纯函数测试
覆盖默认值、环境变量、CLI 覆盖与 inline/agent 冲突。
