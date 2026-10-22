# qaqh-tui 性能优化交接

> 交接日期：2026-10-22。本文记录本轮会话已实施的内容和下一阶段建议；源码与最新测试结果优先于本文。
> **下一阶段“性能模式专属增量文本渲染”尚未实施。不要把设计建议当作已有能力。**

## 1. 用户约束（接手前必读）

1. **禁止使用子代理**：用户明确反馈本版本子代理有 bug。全部工作在主会话完成；不要 spawn / follow-up 子代理。
2. **不强制 release**：用户更希望持续做实际优化，避免额外编译成本。默认 debug、离线、单 job 增量测试/构建；需要 release 时再说明理由。
3. **性能模式的显示简化不得全局生效**：下一阶段纯文本/按行渲染只能由本地性能模式启用；动效模式保留完整 Markdown/语法高亮。
4. **本地渲染设置不回传 daemon**：不得加入协议 ConfigDto/ConfigPatch，也不得通过 config.save/config.load 保存本地项。
5. 用户终端不方便使用 Ctrl+,。主要入口是 **F10 或 /settings**。
6. 不重启用户当前运行的实例、不改用户真实配置、不改系统 swap/内核参数，除非用户另行要求。
7. 用户与代理共享一个已有脏工作区；本轮没有提交。不要 reset/清理不属于当前任务的变更。

## 2. 硬件和观测边界

首次诊断的实测：

- Intel Pentium G2020 @ 2.90GHz，2 核 2 线程，3 MiB L3；SSE4.2，无 AVX/AVX2。
- 约 7.6 GiB RAM，当时 available 约 5.8 GiB；无 swap。
- 约 238.5 GiB 光威 SSD，ext4 分区使用率约 13%。
- Ubuntu 26.04.1 LTS / Linux 7.0.0-38-generic。
- exec 在 bwrap 进程隔离环境中，ps 看不到宿主 TUI/daemon，**没有宿主 CPU/RSS 基线**。
- 用户反馈：第一阶段之后闲时 CPU 约为原来的 **1/4**；工作态暂未看到明显变化，仍在观察。这是用户观测，不是代理受控基准，也不是整机性能承诺。
- 用户反馈过“一直闪”；已定位并移除普通状态更新触发的实际终端清屏，尚无用户对最新构建的视觉复测反馈。

## 3. 已完成的修复

### 3.1 第一阶段：空转与请求风暴

- `src/terminal/agent/mod.rs`：DrawGate 负责首帧、消息批次 dirty、动画停止后的收尾帧。
- Tick 本身不置脏；鼠标 hover 目标不变时不重复绘制。
- `src/app/mod.rs`：needs_draw 不再因无条件全局帧号推进而返回 true。
- handle_tick 显式处理 toast 过期、分钟时钟、首页示例轮换、退出武装过期、配对码倒计时、400ms 工作提示边界。
- 保留 200ms 空闲维护 Tick；完整动画期仍是 60ms Tick。**这不是完全无定时唤醒。**
- `src/app/session_ops.rs`：SessionListRefresh 覆盖 list + activity 整轮在途；失败 3/6/12/24/30s 退避；每类后台只读请求 10s 局部超时。不影响 compact/undo 等长命令。
- 补齐两个既有 CompactFinished 测试构造的 `reason: None`，解除测试编译阻塞。

### 3.2 历史渲染行共享与窗口输出

- `src/terminal/agent/fullscreen.rs`：FullscreenTranscriptCache 的块行使用 `Arc<Vec<Line<'static>>>`。
- segments 只保存共享行与累计起点，不再维护一份深拷贝的全历史 lines 数组。
- `window()` 二分定位首段，然后只复制可见窗口；`line_at()` 支持命中锚点读取。
- register_agent_messages 二分跳过屏外 span，只遍历与视口相交的命中区域。
- 最近 4 个会话的渲染缓存 LRU 保留；**不是 timeline 模型 LRU，也不是活跃会话内存字节上限。**
- 当前仍保存已加载块的渲染行，不是只物化视口附近内容的完整虚拟化。

### 3.3 本地“渲染：动效模式 / 性能模式”

入口：F10 或 /settings → TUI 本地（不发送 daemon）→ 渲染。

- Enter/←→ 切换，s 保存并立即生效，Esc 取消草稿；默认动效模式。
- `src/local_config.rs`：独立 LocalConfig / Store。
- Linux/macOS：`${XDG_CONFIG_HOME:-$HOME/.config}/qaqh-tui/config.toml`。
- Windows：`%APPDATA%\qaqh-tui\config.toml`（未实机测试 Windows 文件替换）。
- 文件当前唯一字段：

```toml
performance_mode = true
```

- 未知字段、格式错误、超过 64 KiB、文件符号链接和专属目录符号链接会报错，保存失败保留草稿和旧运行模式。
- 同目录独占临时文件 + sync_all + rename 替换；Unix 新文件权限 0600。不要宣称这些检查是无 TOCTOU 的安全沙箱。
- SettingsState.performance_draft/local_loaded 与 daemon ConfigPatch 分离。
- **只保存本地项**时，没有 config.save/config.load，也没有后台 API 任务；已有消息队列为空的隔离测试。
- 同时编辑服务端字段时，服务端字段仍通过既有脏字段保存路径发送；本地字段不混入 payload。
- 真实用户配置尚未被本轮工具修改，只有临时测试目录产生过 config.toml。

性能模式当前只改变动画/滚动，**正文仍是完整 Markdown**：

- 固定 `●`，1 秒正常 + 1 秒暗色，两秒一周期。
- 参考 `/home/qaqtamsy/occ/src/components/Spinner/SpinnerGlyph.tsx:8–9,36–40`。
- `src/app/anim.rs` 的 reduced_dot_dim/working_indicator；思考行和侧栏共用。
- 关闭文字 shimmer、首页入场扫光、选中渐显、滚动 tween。
- 不维持 60ms 动画 Tick，圆点相位变化才置脏；正文和输入事件不被限速。
- `QAQH_TUI_ANIM=0|false|off` 优先让圆点静止。

### 3.4 第二阶段第一批：移除清屏闪烁 + 按需块适配

#### 差分绘制

- 已删除 App.force_redraw 字段与所有调用；普通更新统一使用 redraw_requested。
- 用量、压缩进度、team、账号、工具/思考展开等普通变化不再调用 Terminal.clear。
- run_loop 只调用差分 draw_and_publish；pager 返回/终端生命周期恢复仍保留必要清屏。
- draw_and_publish 泛型化以便用真实 CrosstermBackend<Vec 写入端> 做输出回归。
- `Clear` widget 清理帧 buffer 不等于发送终端整屏清除，不能机械删除所有 Clear widget。

#### 按需块适配

- `src/ui/v2/adapter.rs` 新增 BlockSource（借用用户回合/模型块，小系统卡为 owned）及 fullscreen_sources。
- 版本变化时扫描轻量身份/revision/展开状态；**命中渲染缓存不再复制正文/工具输出，不运行 from_tool/normalize_cr**。
- 只有 miss 才 materialize 为 TranscriptBlock，并渲染。
- 不是常驻第二份全历史 view model，避免用内存换掉一次复制后反而存更多正文。
- 查询族分组资格由 `src/ui/v2/transcript.rs::lookup_group_key` 共用。
- 全量参考路径 from_model_with_expanded_blocks/splice_compaction_marks 目前是 cfg(test)，用于逐行逐样式比对。
- 仍有 O(块数) 的借用源扫描、键/段/span 元数据重建，不要声称已经 O(新增字节) 或完全虚拟化。

#### 缓存正确性补强

- FullscreenTranscriptKey / FullscreenBlockKey 纳入 rebaseline_epoch。
- 用户回合头墙钟纳入块键，解决迟到时间回填。
- 有界合成系统卡内容参与键，防等长压缩摘要改写误命中。
- BlockOpened 原地替换已有块时推进 rev。
- 已封口 turn_id 原地 reopen 时推进代际，避免复用旧用户/正文缓存。
- 只按 rev 缓存的前提：所有可见块修改必须 touch，不能直接改 text/tool 而不推进身份。

## 4. 重要源码与回归测试导航

行号是交接时快照，后续修改后可能移动：

| 位置 | 用途 |
|---|---|
| src/terminal/agent/mod.rs:267 | 普通更新不清屏的主循环路径 |
| src/terminal/agent/mod.rs:312 | DrawGate |
| src/terminal/agent/mod.rs:413 | 泛型 draw_and_publish，差分绘制与命中图发布 |
| src/terminal/agent/mod.rs:1876 | normal_updates_emit_diffs_without_terminal_clear_sequences |
| src/ui/v2/adapter.rs:134 | BlockSource::materialize |
| src/ui/v2/adapter.rs:191 | fullscreen_sources |
| src/terminal/agent/fullscreen.rs:940 | window，仅复制可见行 |
| src/terminal/agent/fullscreen.rs:1125 | FullscreenBlockKey::from_source |
| src/terminal/agent/fullscreen.rs:1616 | 200 块历史尾更新只多物化 1 块、199 块继续共享行 |
| src/terminal/agent/mod.rs:3152 | 分组/展开/压缩/等长改写/窄宽度对照全量结果 |
| src/terminal/agent/mod.rs:3176 | 时间回填/块替换/turn reopen 缓存失效 |
| src/terminal/agent/mod.rs:3220 | 同 revision 快照替换与历史分页 |
| src/terminal/agent/mod.rs:3271 | typed truncation 分组、live reasoning 压缩锚 |
| src/app/settings_ops.rs:309 | F10 无 Ctrl+, 的保存/回显/取消路径 |
| src/app/settings_ops.rs:362 | 本地-only 保存不发运行时消息 |
| src/ui/v2/transcript.rs:546 | render_assistant，当前仍整块 sanitize + Markdown |
| src/ui/v2/markdown/code.rs:39 | 代码块整块高亮与特殊 qaqh-ui 占位处理 |

## 5. 最近一次验证（已完成）

- 全量测试：**570 passed, 0 failed**。
- cargo check / cargo build（debug）成功。
- `target/debug/qaqh-tui --version`：`qaqh-tui 2.0.0-beta.2`。
- cargo fmt --check / git diff --check 通过。
- QAQH_TUI_ANIM=0 下 lazy 缓存、无清屏输出、200 块共享/物化专项测试通过。
- 剩余 warning：上游 qaqh-client::params_object 未使用，以及测试 helper render_misses_for_test 未使用；未为压 warning 扩大修改范围。
- 没有 release build，没有重启用户实例，没有宿主 CPU/RSS 受控对照。

复现命令（工作区根目录，命令必须获得当前执行环境授权）：

```bash
TMPDIR="$PWD/target" cargo test --offline --locked -j 1 -- --test-threads=1 --quiet
TMPDIR="$PWD/target" cargo check --offline --locked -j 1
TMPDIR="$PWD/target" cargo build --offline --locked -j 1
cargo fmt -- --check
git diff --check
```

TMPDIR 指向已有 target 是为了绕过当前沙箱 /tmp 或链接器临时目录只读问题；之前 3 个文件测试在默认临时目录因 EROFS 失败，换此路径后全通过。它不是产品代码所需设置，不要写进用户永久环境。

## 6. 尚未完成的热点

1. 正在增长的助手文本每次 miss 仍会整块 sanitize、Markdown 解析和换行。
2. 代码块每次渲染重建 syntect HighlightLines 并走完整块。
3. timeline 变化仍有全历史轻量元数据扫描与段重建；todo 投影也仍按模型版本扫描。
4. 未对内容流绘制做独立帧预算；主队列仍无界，每轮抽干策略未改。
5. 输入线程 poll 10ms 的空闲唤醒未改（有 pager/终端读锁约束）。
6. 缓存仅会话数上限 4，没有活跃会话字节预算。
7. 未做真实性能采样或 profiler，不能保证下一热点的占比。

## 7. 下一阶段：仅性能模式启用轻量增量文本（设计，未落地）

### 7.1 用户问题与原则

用户提出“直接按行渲染”，并明确 **只能加入特化性能模式，不得全局共享简化效果**。

两种行必须区分：

- 逻辑行：原文的换行符，确实存在；原文没有换行时也能是一条很长的逻辑行。
- 视觉行：根据终端列宽、字素宽度、缩进和前缀折出来的显示行。

折行必然有成本。优化目标不是不算，而是 **旧的稳定部分不反复从头算，主要处理新增片段和不稳定尾部**。全量 `starts_with` / 全文 sanitize / 全文宽度统计放在每次追加判定上，也会悄悄把复杂度退回 O(全文)。

### 7.2 推荐的第一版范围

- 默认先针对助手 text block；用户消息、工具卡、交互卡的语义不应顺手改掉。
- 动效模式继续当前完整 Markdown/表格/代码高亮路径。
- 性能模式为助手正文走独立轻量文本布局，不持续运行完整 Markdown/syntect。
- 第一版可以保留普通 Markdown 原始符号或做有限且明确的表现，但需要明确文档说明显示简化。
- **特殊 `qaqh-ui` 围栏不能直接泄漏 DSL 原文**：当前 code.rs:45–49 有明确占位契约。轻量路径仍需识别并显示占位；围栏信息可能跨多个流式片段，不能仅在已闭合代码块上处理。
- 保留原始模型文本，复制 Markdown 与导出继续使用权威原文，不能把显示简化写回 timeline。
- 不建议封口时自动切回完整 Markdown，避免一次解析峰值、行高突变和滚动跳动。这是上轮代理建议，**用户尚未明确确认最终封口策略**；若准备采用不同策略，先解释取舍。

### 7.3 缓存与失效设计要点

1. “渲染模式”必须纳入**外层和块级缓存身份**。现在键里尚无模式字段，因为模式此前只改变动画。只改外层键或只改块键都可能误命中。
2. 需要独立可更新的文本布局状态；仅在 render_block 内换成纯文本函数，但每次从头 wrap 整块，不是完成增量优化。
3. 优先通过 reducer 提供的变更信息分辨 Append / Replace / Reset；避免渲染层猜网络事件、另造协议。
4. TextDelta 是追加；BlockCheckpoint/BlockOpened/快照/undo/压缩可能替换，必须有安全全量回退。
5. 相同块 ID 的 reopen、rebaseline、历史分页、模式切换与展开状态都要正确失效和逐出。
6. resize 可触发完整重排，先保证正确；无 resize 的正常追加才要求避免重复扫描旧文本。
7. 固定前缀、缩进、可用列宽与右侧滚动条必须使用当前生产布局事实，不能另算一套宽度。
8. 避免新增另一份完整正文和一份完整平铺行：复用现有 Arc 段/窗口输出，并检查长期追加内存是否累积。

### 7.4 Unicode 与流式边界

- 按字素/列宽折行，不按字节数或 Unicode scalar 个数直接截。
- 中文通常两列；组合标记、变体选择符、emoji/ZWJ、regional indicator、零宽字符需要测试。
- 新片段可能延长前一个字素：留不稳定尾部回算；不要把每个片段末尾永久冻结。
- CRLF 跨片段、单独 CR 的显示语义、Tab stops、空行与末尾换行要显式定义。
- ANSI/OSC/控制序列仍要清洗；转义序列可跨片段，不能因增量优化绕过已有安全清洗。
- 一条超长无换行文本不能迫使每次追加都扫描完整逻辑行；同时要正确处理字素尾部。
- 完整 Markdown 的标题/表格/列表/引用定义具有跨行回溯语义，不能在动效模式简单用“遇到 \n 即冻结”替代完整解析。

### 7.5 验收与测试

必须有“工作量”证据，而不仅仅是 wall-clock 耗时：

- 固定宽度长文本连续追加：统计处理字节/字素数，稳定前缀不重复布局。
- 分片与一次性文本结果一致：随机/枚举分片边界，覆盖中英文、emoji/组合、换行和控制序列。
- 高度/窗口/滚动条/点击命中与画面一致，新增文本不会误选旧消息。
- 同一内容性能模式下不调用 Markdown/syntect；动效模式输出逐行逐样式与旧路径一致。
- 模式切换有真实重排，无跨模式缓存命中；Esc 不改变运行模式，保存后立即改变。
- 快照同 rev 不同正文、checkpoint 替换、turn reopen、宽度变化、历史前插、seal 都不能显示陈旧行。
- 特殊 qaqh-ui 围栏始终占位，不暴露 DSL，即使开始/结束标记跨多个分片。
- 超长无换行文本、超长代码、滚动到顶部时尾部继续追加，不做不必要可见区重建。
- 不回归无清屏输出和本地设置不回传 daemon 的测试。

## 8. 本轮工作区范围与交接建议

交接时 git status 显示修改涵盖 Cargo.toml/Cargo.lock、README、app 的 anim/mod/session/session_ops/settings/settings_ops/transcript_ops/account_ops/remote_ops/team/timeline_model、main、terminal/agent/fullscreen 与 mod、ui/v2 adapter/transcript/sidebar/workspace；新增 src/local_config.rs。本 handoff 也为新增文件。

建议接手顺序：

1. 读用户约束、看 git status，不使用子代理。
2. 针对文本追加信息与渲染缓存设计一个最小切片，不同时重写队列、Markdown 和所有工具卡。
3. 先补独立布局器及分片等价/工作量测试，再接性能模式。
4. 加模式缓存身份与替换回退；复跑全量测试和无清屏回归。
5. debug 增量构建，给用户试用；明确这是新一阶段，不声称已有 CPU 百分比。
