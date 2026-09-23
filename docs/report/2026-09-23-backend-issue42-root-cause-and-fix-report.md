# 后端 #42 根因与修复报告（跨仓）

> 日期：2026-09-23
> 触发：TUI issue **#42**（timeline SSE live 序列缺 seq 1/2 → v2 transcript 永不渲染）
> 修复位置：`/home/qaqtamsy/项目/qaqh-backend-fix42`（专用 worktree，**未提交**，见 §5）
> 基线：`betav2 @ 9e63c79`（当前后端 HEAD，含 P4 沙箱）

## 0. 结论

**根因不是「丢帧」**——seq 1、2 **从来没有被投递过**，因为它们在**新会话首回合开始时**被一条
「投影重建」路径**凭空消耗**掉了：

```text
新会话首个回合开始
  → ensure_timeline_loaded(seed)：timeline 文件还不存在 → None 分支
  → rebuild_timeline_from_messages(seed)
       此时 messages.jsonl 里已经有**正在进行的**首条用户消息
       → 重建出一个「只有用户文本、没有任何 block」的空回合
       → seq 1 = TurnOpened
       → seq 2 = TurnSealed
       → seal_turn_with_state 的**即时裁剪**把该回合的 journal 条目全删
         （设计如此：sealed 内容已在快照里物化，回放不再需要）
  → restore(snapshot{watermark: 2}, journal: [])
  → 真正的 live 回合只能从 seq 3 起
  → 客户端 cursor=0 按 `cursor + 1` 判 gap → 进入 gap→快照恢复循环 → transcript 空
```

即：**seq 空间被消耗，但那些 seq 既没有 live 投递、也不在 journal 里**——
可观测序列因此天然不连续，而客户端（正确地）把它当成 gap。

## 1. 探针证据（决定性）

在 daemon 里临时插桩（`publish_timeline` / `next_entry` / `journal` / `snapshot` /
`restore` / `ensure` / `rebuild`），跑「先订阅 SSE，再发一条消息」的裸探针：

```text
[PROBE-ensure] seed=a08a1f91 in_memory=false
[PROBE-rebuild] seed=a08a1f91 进入 rebuild_timeline_from_messages
[PROBE-subscribe] seed=a08a1f91 after=0 replay_seqs=[]
[PROBE-ensure] seed=a08a1f91 in_memory=false
[PROBE-rebuild] seed=a08a1f91 进入 rebuild_timeline_from_messages
[PROBE-next_entry] seq=1 turn=t1
[PROBE-journal]   seq=1 len_after_push=1 bytes=0
[PROBE-journal]   seq=1 len_after_budget=1
[PROBE-next_entry] seq=2 turn=t1
[PROBE-journal]   seq=2 len_after_push=2 bytes=0
[PROBE-journal]   seq=2 len_after_budget=2
[PROBE-snapshot]  seed=a08a1f91 next_seq=2 journal_len=0      ← journal 被清空
[PROBE-rebuild]   重建结果 watermark=2 journal_len=0
[PROBE-restore]   watermark=2 journal_len=0
[PROBE-next_entry] seq=3 turn=t1                              ← live 第一条
[PROBE-publish]   seq=3 turn=t1 kind=TurnOpened
```

三个关键点：

1. **`[PROBE-publish]` 里根本没有 seq 1/2** —— 不是「发布了但被过滤」，是**从未发布**；
2. `[PROBE-drop]` 一次都没出现 —— `should_deliver_timeline_live` 的三个子句
   （seed 归属 / `> after` / `replayed` 去重）**全部通过**，投递过滤是无辜的；
3. journal 在 seq 2 之后、snapshot 之前被清空，且**没有新的 seq 分配** ——
   与 `seal_turn_with_state` 里 `prune_turn_journal`（seal 即时裁剪）完全吻合。

这也解释了报告里三个候选根因中**哪一个是真**：不是 hub 的订阅/replay 窗口，
也不是 `leases.owns_seed`，而是候选 ②「seq 属于不可投递 entry 但仍占用了 next_seq」。

## 2. 修复

一处、约 20 行，`crates/qaqh-runtime/src/ringing/timeline_hub.rs` 的
`ensure_timeline_loaded` 无快照分支：

```rust
None => {
    // 无快照。BUG-006 的重建是给「**已有历史**但 timeline 文件缺失/损坏」用的
    // 恢复路径，不是给新会话用的。
    //
    // #42：新会话首个回合开始时 messages.jsonl 里已经有正在进行的首条用户消息，
    // 重建会物化一个空回合并吃掉 seq 1..2（seal 即时裁剪后 journal 为空、也不曾
    // live 投递），使真正的 live 回合从 seq 3 起 —— 客户端 cursor=0 判 gap。
    //
    // meta.turn_count 是**已完成**回合数的权威值：为 0 说明没有可恢复的历史，
    // 交给 live 路径物化即可，绝不能在这里凭空消耗 seq 空间。
    let completed_turns = self.persisted_turn_count(seed).unwrap_or(0);
    if completed_turns > 0 && self.rebuild_timeline_from_messages(seed) {
        self.enable_turn_offload(seed);
    }
    return;
}
```

**为什么是这个判据**：

- 重建的目的是「有历史、但 timeline 文件丢了」的恢复；`turn_count == 0` 时
  没有可恢复的历史，重建只会把**正在进行**的首回合误当成历史来物化；
- 取不到 meta 时按 0 处理 —— 同样没有可恢复的历史，而 live 路径始终会写；
- 没有改 seq 分配、没有改裁剪策略、没有改投递过滤 —— 风险面极小。

## 3. 验证

### 3.1 裸探针：live 序列从 1 起、连续

```text
修复前：3,4,5,6,7,8,9      （7 帧，缺 1、2）
修复后：1,2,3,4,5,6,7      （7 帧，连续）
```

### 3.2 TUI 端到端

```text
DAEMON=<fix42>/target/debug/qaqh-daemon MODE=none bash scripts/e2e-v2-faults.sh
  [✓] 基线：用户消息可见
  [✓] 基线：回复可见          ← #42 的直接症状，修复前红
  [✓] no cursor-position timeout
  [✓] no panic
RESULT: PASS
```

### 3.3 故障钩子套件（#42 原本阻塞的就是它）

| MODE | 修复前 | 修复后 |
|---|---|---|
| `none` | ✗ | **✓ PASS** |
| `ack-delay` | ✗ | **✓ PASS** |
| `ack-hang` | ✗ | **✓ PASS** |
| `lagged` | ✗ | ✗（见 §4.1） |
| `gap` | ✗ | ✗（见 §4.2） |
| `session-404` | ✗ | ✗（见 §4.1） |

### 3.4 交互套件无回归

```text
MODE=permission    PASS
MODE=plan-reject   PASS（9 条后端证据）
```

## 4. 剩余失败的性质（**都不是本次修复引入**）

### 4.1 `lagged` / `session-404`：UI 文案断言

```text
[✗] lagged 诊断文案可见
[✓] 重连后会话仍可用（回复可见）      ← 行为是对的
[✗] 404 提示可见
[✓] 非子会话未被关闭（仍能干净退出）   ← 行为是对的
```

两条**行为**断言都通过，红的是「某个诊断文案是否出现在屏幕上」——属于 **TUI 侧**
harness 断言与 v2 实际渲染文案的匹配问题（v1/v2 文案或渲染通道不同），不是后端缺陷。

### 4.2 `gap`：注入 gap 后回复不可见 —— **已定位并修复（第二处）**

```text
[✓] gap 后用户消息仍在（re-baseline 生效）
[✗] gap 后回复可见
```

`QAQH_TEST_TIMELINE_GAP=1` 丢弃第一条可投递 entry 后，客户端 re-baseline 生效
（用户消息在），但**该回合的回复仍不渲染**。

**daemon 侧 SSE 探针（修复前）**：

```text
[PROBE-sse] subscribe after=0 gap=true replay_seqs=[]
[PROBE-sse] gap-drop(live) seq=1        ← 钩子丢掉 TurnOpened
[PROBE-sse] send(live) seq=2            ← 客户端收到 2（期望 1）→ 判 gap
[PROBE-sse] send(live) seq=3
[PROBE-sse] send(live) seq=4
[PROBE-sse] subscribe after=3 gap=false replay_seqs=[]   ← 重连后**一帧都没发**
```

**根因**：客户端 gap 恢复时取的快照是**回合中途**的（watermark=3），此后它要靠
`seq > 3` 的条目把这个回合补完；但这些条目在 **turn seal 时被即时裁剪**删掉了，
而重连又晚于它们的 live 投递 —— 客户端**永远收不到 `TurnSealed`**，回合停在未
封口态，回复不渲染。

`persistence_policy.rs` 的文档把 seal 裁剪写成有意设计，代价是「重连走
`recover_gap` 快照重基线」。这条在**回合中途重基线**时并不成立：快照本身就没
覆盖完整回合，裁剪又拿走了补齐所需的那段。

**修复（第二处，`crates/qaqh-runtime/src/timeline.rs`）**：去掉
`seal_turn_with_state` 里的无条件 `prune_turn_journal`（连同死代码一起删除）。

内存上界本来就有两条硬约束，每次 `next_entry` 都会执行：

- `MAX_TIMELINE_JOURNAL_ENTRIES = 8192`（条数）
- `journal_byte_limit()`（字节，实测最坏 256 MB 上限）

seal 裁剪是**冗余的第二道**，却以「最近一个回合不可重放」为代价 —— 去掉它，
内存仍由预算钉死。

**修复后**：

```text
[PROBE-sse] subscribe after=3 gap=false replay_seqs={4,5,6,7}   ← HashSet 打印序
[PROBE-sse] send(replay) seq=4
[PROBE-sse] send(replay) seq=5
[PROBE-sse] send(replay) seq=6
[PROBE-sse] send(replay) seq=7
```

```text
[✓] gap 后用户消息仍在（re-baseline 生效）
[✓] gap 后回复可见
RESULT: PASS
```

### 4.3 修复后故障钩子总览

| MODE | 修复前 | 修复后 |
|---|---|---|
| `none` | ✗ | **✓** |
| `gap` | ✗ | **✓**（第二处修复） |
| `ack-delay` | ✗ | **✓** |
| `ack-hang` | ✗ | **✓** |
| `lagged` | ✗ | ✗（仅 UI 文案断言，行为已通过） |
| `session-404` | ✗ | ✗（仅 UI 文案断言，行为已通过） |

## 5. 修复代码的位置与交接

按「把修复代码混在当前工作区」的要求，修复**未提交**，留在专用 worktree：

```text
/home/qaqtamsy/项目/qaqh-backend-fix42        # detached @ 9e63c79（betav2 HEAD）
  crates/qaqh-runtime/src/ringing/timeline_hub.rs   ← 修复①（seq 空间）
  crates/qaqh-runtime/src/timeline.rs               ← 修复②（seal 裁剪）
```

两处改动合计约 45 行（含注释），互不依赖，可分开合入。

- 该 worktree 是**新开的**，没有碰后端开发工作树（那里有 5 个未提交的沙箱文件），
  也没碰只读锚点 worktree `qaqh-backend-anchor`（TUI 门禁依赖它保持纯净）；
- 取 diff：`git -C ../qaqh-backend-fix42 diff`；
- 验证用 daemon：`/home/qaqtamsy/项目/qaqh-backend-fix42/target/debug/qaqh-daemon`；
- 后端可直接 `git diff > /tmp/fix42.patch` 后在自己分支上 `git apply`。

**未做**：没有跑后端 workspace 全量测试（`cargo test --workspace`，~2min）——
改动面只有一条分支判定，但仍建议后端合入前跑一次。
