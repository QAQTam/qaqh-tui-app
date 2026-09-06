# 流式链路边界行为审计（timeline / SSE / reasoning 渲染）

- 日期：2026-09-06
- 范围：TUI 侧 SSE 解码 → runtime 流任务 → timeline reducer → transcript 渲染；
  交叉对照 daemon（QAQ-Harness `qaqh-runtime`）发送端语义。
- 背景：`6a7356b`（fresh 块接受 fragment_seq=0）与 `50b301e`（CJK 思考链不提升为
  title）两修复后的全面边界复查。

## 一、已验证的保障链（信任根）

1. **整流重摆，非续传**（runtime.rs `timeline_stream`）：每次连接先拉 HTTP 快照并
   `TimelineRebaseline` 全量替换，cursor = watermark；SSE 条目要求严格 `cursor+1`，
   gap / JSON 解析失败 / seed 或 epoch 不匹配 / reset_required → 一律 break 回循环
   顶部重拉快照。**transport 层不存在"丢一条 delta 继续跑"的路径。**
2. **reducer 幂等**：重复/回放条目按 fragment_seq 单调性丢弃；`BlockCheckpoint`
   覆盖语义独立自愈（节奏：每 64 token 或 2s，先到先发，gate.rs
   `CHECKPOINT_TOKEN_INTERVAL=64` / `CHECKPOINT_INTERVAL=2s`）。
3. **daemon 发送端契约**（均已在源码验证）：
   - `open_block` 恒以空文本开块；`append_text` 强制 fragment_seq 严格递增
     （`FragmentOutOfOrder`），首分片 seq=0；
   - `round_num` 在全部 timeline intent 中恒为 `Some`（next_entry 显式携带）；
   - `block_id` 同回合唯一（重复 → `DuplicateBlock` 拒绝）。

## 二、边界清单（按等级）

### A. 协议缺口（跨仓库建议，本次不实施）

**A1. seal 前无最终 checkpoint。**
`seal_active_stream_block` / `seal_timeline_terminal_round`（gate.rs，4 个调用点：
engine_turn.rs L198/225/293/567）直接发 `BlockSealed`，不先发权威全文的
`BlockCheckpoint`。对采用"整流重摆"的消费端（本 TUI）无影响——block 文本 =
全部 delta 之和，天然权威；对采用"增量 + checkpoint 自愈"的消费端（winui web
形态），最后一次 checkpoint 之后的增量若被传输层丢弃，**seal 后即永久缺失**。
建议：daemon 在两个 seal 函数发 `BlockSealed` 前补发一次 `BlockCheckpoint`
（全文），把"概率自愈"升级为"保证收敛"，成本一条事件。

### B. 契约破坏时的已知静默点（accepted，附检测建议）

**B1. reducer 对 missing block/turn 的事件静默丢弃**（`changed=false`，无计数）：
`TextDelta`/`ToolProgress`/`BlockSealed` 等在 turn 或 block 不存在时直接丢弃。
实践中仅 daemon 契约破坏（round_num 缺失、事件乱序）或回合被 `cap_turns`
逐出时可达。建议未来：TimelineModel 加 `dropped: u64` 计数，doctor/debug
overlay 可读；或在 App 层对"块缺失的 delta"触发一次 re-baseline 请求。

**B2. 重复 `BlockOpened` 整块替换**：`Some(idx) => round.blocks[idx] = wire` 会
清空已累计文本并重置 last_fragment=0，后续旧 seq delta 以空文本为基重放
（重复风险）。daemon 契约上不可达（DuplicateBlock 拒绝 + 整流重摆不重放
BlockOpened）。若未来引入"流内重开块"语义，此处必须先改。

### C. SSE 解码器（accepted）

- **C1** 非法 UTF-8 行整行跳过（绝不 lossy，保护中英文完整性）——设计行为；
- **C2** 永不终止的单行（daemon bug / 恶意上游）→ buf 无界增长
  （COMPACT_THRESHOLD 只搬移已消费前缀）。daemon 可信，防御性上限
  （如单行 1MB 熔断）留待未来；
- **C3** 流首 BOM 会使首行 `id:`/`event:` 字段丢失（服务端不发送，现实中不可达）。

### D. 渲染层（accepted，均 cosmetic）

- **D1** `is_gerund_word` 对 "sing"/"thing"/"king" 等 4 字母 -ing 词误报 gerund，
  理论上可把此类英文首行提升为标题（仅标题判定，无内容丢失）；
- **D2** 流式 text 块原样渲染 `\r`（LLM 正文罕见输出 \r，bash 进度流已有
  `apply_bash_progress` 净化，text/reasoning 无）；
- **D3** 块 sealed 瞬间从纯文本切换 markdown 渲染，`**`/`##` 等标记字形消失
  ——渲染语义而非丢字，注意与真吞字区分（真吞字已由 `6a7356b` 根除）；
- **D4** 单块 markdown > 500 行截断并提示（设计，防 100M 爆存）。

### E. 复查确认无问题

- `SSE_IDLE_TIMEOUT=45s` vs daemon"闸门关闭旧流"策略：两条恢复路径
  （空闲判死重连 / 服务端主动关流）都进入同一整流重摆循环，无错配；
- `cap_turns` 窗口：活动回合恒在窗口尾，不会被逐出；逐出回合的迟到条目
  由整流重摆兜底；
- `prepend_older` 单回合边界假设（before_turn 锚点）成立；
- 空 delta 为 no-op；多 `data:` 行聚合后 JSON 解析失败 → recover 兜底；
- `ToolProgress` 先于 `ToolUpdated` 的乱序在 daemon 契约上不可达，且
  `ToolUpdated` 全量替换 tool 卡（含 progress）天然自愈；
- checkpoint 与 last_fragment 双计数独立，互不重置——设计正确。

## 三、结论

两个用户可见缺陷（首行吞字、思考链同行）已修复入库；其余边界全部收敛于
"整流重摆"这一信任根，无本次需修项。最有价值的后续改进是 A1（daemon seal
前补最终 checkpoint，一行级改动、跨端收益）与 B1（丢弃可观测化）。
