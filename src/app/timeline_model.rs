//! timeline reducer：timeline 事件的唯一消费者（对照 winui `block_transcript.rs`
//! 的语义，修正为幂等可重放）。
//!
//! 快照（bootstrap/分页）整体替换模型；SSE 条目按严格 +1 光标送达（transport
//! 层保证），但 reducer 本身必须对重复应用幂等——undo 后的重取、断点续传的
//! 回放都可能造成重复条目。

use qaqh_client::{
    TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineFailure,
    TimelinePage, TimelineTool, TimelineToolState, TimelineTurn, TimelineTurnState,
};

const MAX_PROGRESS_LEN: usize = 8192;
const PROGRESS_TAIL_KEEP: usize = 6144;

fn retain_utf8_tail(value: &mut String, max_bytes: usize) -> bool {
    if value.len() <= max_bytes {
        return false;
    }
    let mut start = value.len() - max_bytes;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value.drain(..start);
    true
}

/// 去除 ANSI/VT 转义（供 bash 进度流净化：ESC[..m/K/J 等）。
/// 不引入 regex 依赖，纯状态机，保留原 UTF-8。
pub fn strip_ansi_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // ESC
            match chars.peek() {
                Some(&'[') => {
                    chars.next(); // '['
                    // CSI: 参数直到终字节 0x40-0x7E
                    while let Some(&nc) = chars.peek() {
                        chars.next();
                        if ('@'..='~').contains(&nc) {
                            break;
                        }
                    }
                }
                Some(&']') => {
                    chars.next(); // ']'
                    // OSC 直到 BEL \x07 或 ESC \
                    while let Some(nc) = chars.next() {
                        if nc == '\x07' {
                            break;
                        }
                        if nc == '\x1b' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some(&'(') | Some(&')') | Some(&'*') | Some(&'+') => {
                    chars.next();
                    chars.next();
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// 将一次 ToolProgress chunk 合并到 progress 缓冲。
/// 语义：`\n` 换行；`\r` 回车覆写当前行（取最后非空 `\r` 段）；`\r\n` 视为换行；
/// ANSI 已剥离；末尾超长截断留尾。`\r` 仅在同 chunk 内覆写，跨 chunk 的
/// 连续覆写需同 chunk 内完成（符合 8KB/50ms 批的实际，切块边界不跨行）。
pub fn apply_bash_progress(buf: &mut String, chunk: &str) -> bool {
    if chunk.is_empty() {
        return false;
    }
    let cleaned = strip_ansi_escapes(chunk).replace("\r\n", "\n");
    if cleaned.is_empty() {
        return false;
    }
    // 按 `\n` 切，保持尾空以便还原末尾换行
    let parts: Vec<&str> = cleaned.split('\n').collect();
    for (idx, part) in parts.iter().enumerate() {
        let is_last = idx == parts.len() - 1;
        let is_tail_empty = is_last && part.is_empty() && cleaned.ends_with('\n');
        if is_tail_empty {
            continue;
        }
        let has_cr = part.contains('\r');
        let effective: &str = if has_cr {
            let segs: Vec<&str> = part.split('\r').collect();
            let mut eff = "";
            for s in segs.iter().rev() {
                if !s.is_empty() {
                    eff = *s;
                    break;
                }
            }
            eff
        } else {
            part
        };
        let is_pure_cr = has_cr && effective.is_empty();
        if idx == 0 {
            if has_cr {
                if is_pure_cr {
                    continue;
                }
                if buf.is_empty() {
                    buf.push_str(effective);
                } else if let Some(pos) = buf.rfind('\n') {
                    buf.truncate(pos + 1);
                    buf.push_str(effective);
                } else {
                    buf.clear();
                    buf.push_str(effective);
                }
            } else {
                // 跨 chunk 的 `\r` 覆盖：若 buf 当前行未以 `\n` 结束，说明上一 chunk 是
                // 以 `\r` 结尾的进度行（debian apt），下一个 plain 行应覆写同一行
                let buf_has_active_line = !buf.is_empty() && !buf.ends_with('\n');
                if buf_has_active_line {
                    if let Some(pos) = buf.rfind('\n') {
                        buf.truncate(pos + 1);
                        buf.push_str(effective);
                    } else {
                        buf.clear();
                        buf.push_str(effective);
                    }
                } else {
                    buf.push_str(effective);
                }
            }
        } else {
            buf.push('\n');
            if is_pure_cr {
                continue;
            }
            buf.push_str(effective);
        }
    }
    if cleaned.ends_with('\n') && !buf.ends_with('\n') {
        buf.push('\n');
    }
    buf.len() > MAX_PROGRESS_LEN && retain_utf8_tail(buf, PROGRESS_TAIL_KEEP)
}

#[allow(dead_code)]
pub fn normalize_progress_history(chunks: &[&str]) -> String {
    let mut buf = String::new();
    for c in chunks {
        apply_bash_progress(&mut buf, c);
    }
    buf
}

/// 工具卡（timeline tool 的展示镜像，progress 独立可追加）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCard {
    pub tool_call_id: String,
    pub name: String,
    pub state: TimelineToolState,
    pub summary: Option<String>,
    pub args_json: Option<String>,
    pub output: Option<String>,
    pub diff: Option<String>,
    pub progress: String,
    pub progress_truncated: bool,
    /// 契约 §5.1（P2）：本次调用累计观测字节（含被丢弃/裁剪前）——运行卡
    /// 显示 `↓ 12.3 KB`（终态以 metrics.output_bytes 为准，不重复）。
    pub progress_bytes_total: u64,
    /// 契约 §5.1（P2）：进度流标识（"stdout"|"stderr"|"mixed"；未知按 None）。
    pub progress_stream: Option<String>,
    pub failure: Option<TimelineFailure>,
    pub permission: Option<qaqh_client::TimelineToolPermission>,
    /// 类型化展示投影（09-18 跨仓契约 §3.3）；None → H16 完整回退旧字段。
    pub display: Option<qaqh_client::TimelineToolDisplay>,
}

impl From<TimelineTool> for ToolCard {
    fn from(t: TimelineTool) -> Self {
        let (progress, progress_truncated) = {
            let mut progress_truncated = t.progress_truncated;
            let is_bash = matches!(
                t.name.as_str(),
                "bash" | "exec" | "shell" | "pwsh" | "powershell"
            );
            if is_bash && !t.progress.is_empty() {
                let mut buf = String::new();
                progress_truncated |= apply_bash_progress(&mut buf, &t.progress);
                (buf, progress_truncated)
            } else {
                let mut progress = t.progress;
                progress_truncated |= retain_utf8_tail(&mut progress, MAX_PROGRESS_LEN);
                (progress, progress_truncated)
            }
        };
        Self {
            tool_call_id: t.tool_call_id,
            name: t.name,
            state: t.state,
            summary: t.summary,
            args_json: t.args_json,
            output: t.output,
            diff: t.diff,
            progress,
            progress_truncated,
            progress_bytes_total: t.progress_bytes_total,
            progress_stream: t.progress_stream,
            failure: t.failure,
            permission: t.permission,
            display: t.display,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub block_id: String,
    pub block_order: u32,
    pub kind: TimelineBlockKind,
    pub state: TimelineBlockState,
    pub text: String,
    pub tool: Option<ToolCard>,
    /// TextDelta 的单调 fragment 计数（BlockCheckpoint 不重置）。
    pub(crate) last_fragment: u64,
    /// 渲染修订号：**只要该块的可见内容可能变化就自增**。
    ///
    /// 渲染缓存以 `(block_id, rev, width, …)` 为键——rev 未变即内容未变，
    /// 可直接复用上帧的 `Arc<[RenderLine]>`，不必重跑 markdown/syntect。
    /// 用显式计数而非内容哈希：哈希是 O(text)，而这里要的是 O(1)。
    pub(crate) rev: u64,
}

impl Block {
    fn from_wire(b: TimelineBlock) -> Self {
        Self {
            block_id: b.block_id,
            block_order: b.block_order,
            kind: b.kind,
            state: b.state,
            text: b.text,
            tool: b.tool.map(ToolCard::from),
            last_fragment: 0,
            rev: 1,
        }
    }

    pub fn is_streaming(&self) -> bool {
        self.state == TimelineBlockState::Open
    }

    /// 标记内容已变（任何可能影响渲染的写入之后调用）。
    fn touch(&mut self) {
        self.rev = self.rev.wrapping_add(1);
    }

    /// 该块是否含“随时间变化的字形”（spinner / ▌ 光标 / 进度条）。
    ///
    /// T8 后动画已出带（AnimSlot），缓存不再依赖它；保留供 §3.5 帧调度
    /// （dirty 判定）在 M2 接线。
    #[allow(dead_code)]
    pub fn is_animating(&self) -> bool {
        if self.state == TimelineBlockState::Open {
            return true;
        }
        self.tool
            .as_ref()
            .is_some_and(|t| t.state == TimelineToolState::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Round {
    pub round_num: u32,
    pub sealed: bool,
    pub is_final: bool,
    pub blocks: Vec<Block>,
}

/// 思考聚合元数据（§4.6）：回合头显示 `思考 N 段/M 行`，B1 闭环的计数侧。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThinkingStats {
    /// reasoning 块数（「段」）。
    pub segments: u32,
    /// 各 reasoning 块正文的行数合计。
    pub lines: u64,
}

/// D1（Codex 式思考链路）：sealed 回合的 reasoning body 不驻留——统计进
/// [`Turn::thinking`] 后就地清空。唯一保留 body 的地方是**活动回合**（Running）
/// （Ctrl+T 浮层回放当前回合的思考）。
///
/// 幂等：`TurnSealed`、分页重放（`Turn::from_wire`）、re-baseline
/// （`replace_from_page`）三条路都会走到；对已清空的块重复调用是 no-op
/// （text 已空 → 不再计段不计行）。
pub(crate) fn discard_sealed_reasoning(turn: &mut Turn) {
    if !turn.sealed {
        return;
    }
    for round in &mut turn.rounds {
        for block in &mut round.blocks {
            // 只计非空 body：空段无内容可数，也保证重复调用幂等。
            if block.kind == TimelineBlockKind::Reasoning && !block.text.is_empty() {
                turn.thinking.segments = turn.thinking.segments.saturating_add(1);
                turn.thinking.lines = turn
                    .thinking
                    .lines
                    .saturating_add(block.text.lines().count() as u64);
                block.text.clear();
            }
        }
    }
}

/// 单个回合：一次用户输入的全部模型输出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub turn_id: String,
    /// 会话内的**全局回合序号**（后端 `TimelineTurn.turn_index`）——翻页游标用它。
    ///
    /// `turn_id` 当不了游标：它由 worker 计数器生成、**会复用**（后端
    /// `TimelineAppender::open_turn` 明确容忍原地 reopen，注释里记着实测的 `t14`
    /// 重启重号）。实时追加的回合不带序号（`None`），故取游标时要 `and_then`。
    pub turn_index: Option<u64>,
    pub user_text: String,
    pub state: TimelineTurnState,
    pub failure: Option<TimelineFailure>,
    /// 服务端已封口（线协议 `TimelineTurn.sealed`）。
    ///
    /// 不可用 `state != Running` 代替：后端判定「原地 reopen」用的**正是**
    /// `sealed`（`qaqh-runtime/src/timeline.rs:208-244`）。两者一旦分离
    /// （sealed 却仍 Running），用 state 推断就会漏判——那正是本字段所修的
    /// 那类错位。线协议本就携带它，此前在 `from_wire` 被丢弃。
    pub sealed: bool,
    /// T-06：该回合已被 offload —— 常驻内存里只剩「预览壳」：block 正文被截到
    /// 512 字符、`tool.output`/`tool.diff` 被清空（见 `qaqh-runtime/src/timeline.rs`
    /// 的 offload 路径）。**必须让用户看得见**，否则残缺内容会被当成完整回合
    /// ——与 B1「丢弃必须可见」同一设计原则。
    pub offloaded: bool,
    /// 思考聚合（§4.6，B1 载体）：seal 时统计并**丢弃 reasoning body**
    /// （D1 Codex 式：活动区 + Ctrl+T 回放替代 transcript 常驻）。
    pub thinking: ThinkingStats,
    pub rounds: Vec<Round>,
}

impl Turn {
    fn from_wire(t: TimelineTurn) -> Self {
        let mut turn = Self {
            turn_id: t.turn_id,
            turn_index: t.turn_index,
            user_text: t.user_text,
            state: t.state,
            failure: t.failure,
            sealed: t.sealed,
            offloaded: t.offloaded,
            thinking: ThinkingStats::default(),
            rounds: t
                .rounds
                .into_iter()
                .map(|r| Round {
                    round_num: r.round_num,
                    sealed: r.sealed,
                    is_final: r.is_final,
                    blocks: r.blocks.into_iter().map(Block::from_wire).collect(),
                })
                .collect(),
        };
        // D1：分页/re-baseline 重放到达的 sealed reasoning 同样丢 body 只计数
        // （有损客户端纪律，幂等——重复调用对已清空块是 no-op）。
        discard_sealed_reasoning(&mut turn);
        turn
    }

    pub fn is_streaming(&self) -> bool {
        self.state == TimelineTurnState::Running
    }
}

/// timeline 条目携带的 turn 终态（`TurnSealed`）。
///
/// 调用方据此收口 streaming 状态：终态在 timeline 通道上必然可见，因而
/// 不依赖对话频道 `TurnCompleted` 的到达顺序（两者是独立 SSE）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnTerminal {
    pub turn_id: String,
    pub state: TimelineTurnState,
}

/// 单会话 transcript 模型。`version` 每次变更自增，用于渲染缓存。
#[derive(Debug, Clone, Default)]
pub struct TimelineModel {
    pub turns: Vec<Turn>,
    pub has_more: bool,
    pub total_turns: usize,
    /// 已从窗口头部丢弃的回合数（`cap_turns`）。
    ///
    /// 回合显示编号靠它保持稳定：编号 = `dropped_turns + idx + 1`。
    /// 若用裸下标，`cap_turns` 后所有编号集体前移 → 缓存键全失效。
    pub dropped_turns: usize,
    /// T-08：服务端的物化窗口**未覆盖到历史开头**——更早的回合存在（daemon
    /// 归档里），但当前没有深翻页接口能取到。与 `has_more` 分工明确：
    /// `has_more` = 「还能再往前翻一页（且那页非空）」，本字段 = 「翻到底了，
    /// 但历史并不止于此」。二者可以同时为真（窗口内还能翻，翻到头仍够不到开头）。
    pub truncated_before: bool,
    pub version: u64,
    /// 权威快照整体替换的代际；undo/compact/rebaseline 用它触发 terminal
    /// scrollback purge 与 projector/ledger 重建。
    pub rebaseline_epoch: u64,
    /// B1 可观测：事件引用的 block/tool 卡缺失被丢弃的次数（契约异常信号）。
    /// 设计内幂等丢弃不计入：旧 fragment_seq 重放、快照窗口外迟到条目。
    pub dropped_missing_block: u64,
    /// B1 可观测：事件引用的 turn 缺失被丢弃的次数（快照窗口外迟到条目，
    /// re-baseline 自愈；持续增长 = 契约破坏或窗口配置异常）。
    pub dropped_missing_turn: u64,
}

impl TimelineModel {
    fn bump(&mut self) {
        self.version += 1;
    }

    /// B1 可观测：非零丢弃时返回紧凑摘要（status_bar 展示），恒零返回
    /// None——正常会话该信号必须完全不可见（零噪声设计）。
    pub fn dropped_summary(&self) -> Option<String> {
        if self.dropped_missing_block == 0 && self.dropped_missing_turn == 0 {
            return None;
        }
        Some(format!(
            "⚠ dropped b={} t={}",
            self.dropped_missing_block, self.dropped_missing_turn
        ))
    }

    fn find_turn_mut(&mut self, turn_id: &str) -> Option<&mut Turn> {
        self.turns.iter_mut().find(|t| t.turn_id == turn_id)
    }

    fn find_round_mut(turn: &mut Turn, round_num: u32) -> &mut Round {
        if let Some(idx) = turn.rounds.iter().position(|r| r.round_num == round_num) {
            return &mut turn.rounds[idx];
        }
        turn.rounds.push(Round {
            round_num,
            ..Round::default()
        });
        let idx = turn
            .rounds
            .iter()
            .position(|r| r.round_num == round_num)
            .expect("just pushed");
        &mut turn.rounds[idx]
    }

    fn find_block_mut<'a>(round: &'a mut Round, block_id: &str) -> Option<&'a mut Block> {
        round.blocks.iter_mut().find(|b| b.block_id == block_id)
    }

    /// 应用一条 timeline 条目（幂等）；返回该条目携带的 turn 终态（若有）。
    pub fn apply(&mut self, entry: &TimelineEntry) -> Option<TurnTerminal> {
        use qaqh_client::TimelineEvent as E;
        let turn_id = entry.turn_id.as_str();

        if let E::TurnOpened { user_text } = &entry.event {
            // 用索引而非 `find_turn_mut` 的借用：reopen 分支改完字段后还要
            // 调 `self.bump()`，借用必须在该调用前结束。
            match self.turns.iter().position(|t| t.turn_id == turn_id) {
                None => {
                    self.turns.push(Turn {
                        turn_id: turn_id.to_owned(),
                        // 实时事件不带全局序号：分页游标只服务历史（参见 `Turn::turn_index`）。
                        turn_index: None,
                        user_text: user_text.clone(),
                        state: TimelineTurnState::Running,
                        failure: None,
                        sealed: false,
                        thinking: ThinkingStats::default(),
                        // 实时新建的回合不可能已 offload（offload 只作用于已封口的
                        // 回合的重载路径）。
                        offloaded: false,
                        rounds: Vec::new(),
                    });
                    self.bump();
                }
                // 镜像后端「原地 reopen」（`qaqh-runtime/src/timeline.rs:208-244`）：
                // daemon 重启后 worker 的 turn 计数可能滞后，于是把**已 sealed** 的
                // turn_id 复用给新输入；后端此时原地重置该回合（换 user_text、
                // sealed=false、state=Running、rounds.clear()，并重置 fragment
                // 计数），而非报 `DuplicateTurn`。
                //
                // 不同步这一步的后果：TUI 保留上一轮的 rounds，新流入的内容与旧
                // 内容混在同一回合；且 state 停在终态 → `running_turn_id()` 不认它，
                // 流式指示与后续收口全部错位（后端称之为「同一族错位症状」）。
                Some(idx) if self.turns[idx].sealed => {
                    let turn = &mut self.turns[idx];
                    turn.user_text = user_text.clone();
                    turn.state = TimelineTurnState::Running;
                    turn.failure = None;
                    turn.sealed = false;
                    turn.thinking = ThinkingStats::default();
                    // 清 rounds 同时丢弃 per-block `last_fragment`——等价于后端
                    // 重置 `next_fragment`，使续流复用块 id 时 seq=0 不被拒。
                    turn.rounds.clear();
                    self.bump();
                }
                // 运行中的重复 `TurnOpened`：后端不会发出（会返回 `DuplicateTurn`），
                // 只可能是快照+回放的幂等重投，保持 no-op。
                Some(_) => {}
            }
            return None;
        }

        let turn = match self.find_turn_mut(turn_id) {
            Some(turn) => turn,
            // 快照窗口之外的迟到条目：忽略（re-baseline 会补齐权威状态），
            // 但计入 B1 可观测信号——持续增长意味着契约破坏或窗口异常。
            None => {
                self.dropped_missing_turn += 1;
                return None;
            }
        };

        // 本条目携带的 turn 终态（TurnSealed）。
        let mut terminal: Option<TurnTerminal> = None;
        let mut changed = true;
        // match 内 turn 可变借用存活，无法直接触碰 self——missing-block 丢弃
        // 先记局部计数，match 后再汇总到 self（与 changed 标志同套路）。
        let mut missing_block_drops: u8 = 0;
        match &entry.event {
            E::TurnOpened { .. } => unreachable!(),
            E::BlockOpened { block } => {
                let round = Self::find_round_mut(turn, entry.round_num.unwrap_or(0));
                let wire = Block::from_wire(block.clone());
                match round
                    .blocks
                    .iter()
                    .position(|b| b.block_id == wire.block_id)
                {
                    Some(idx) => round.blocks[idx] = wire,
                    None => {
                        // 按 block_order 插入，保持块序稳定。
                        let pos = round
                            .blocks
                            .iter()
                            .position(|b| b.block_order > wire.block_order)
                            .unwrap_or(round.blocks.len());
                        round.blocks.insert(pos, wire);
                    }
                }
            }
            E::TextDelta {
                block_id,
                fragment_seq,
                delta,
            } => {
                let round_num = entry.round_num.unwrap_or(0);
                let round = Self::find_round_mut(turn, round_num);
                if let Some(block) = Self::find_block_mut(round, block_id) {
                    // 单调 fragment 计数：重复/回放的增量被丢弃。
                    // daemon 契约：open_block 恒空文本开块、首分片 seq=0 且严格
                    // 递增（append_text 校验 FragmentOutOfOrder）。fresh 块必须
                    // 接受 seq=0，否则首条 delta 被静默丢弃（流式首行吞字），
                    // 只能等 BlockCheckpoint 自愈；快照带文本的块不算 fresh，
                    // seq=0 回放依旧幂等丢弃。
                    let is_fresh = block.text.is_empty() && block.last_fragment == 0;
                    if *fragment_seq > block.last_fragment || (is_fresh && *fragment_seq == 0) {
                        block.text.push_str(delta);
                        block.last_fragment = *fragment_seq;
                        block.touch();
                    } else {
                        changed = false;
                    }
                } else {
                    missing_block_drops += 1;
                    changed = false;
                }
            }
            E::BlockCheckpoint {
                block_id,
                arg,
                text,
            } => {
                let round_num = entry.round_num.unwrap_or(0);
                let round = Self::find_round_mut(turn, round_num);
                if let Some(block) = Self::find_block_mut(round, block_id) {
                    // 权威语义（`qaqh-runtime` `checkpoint_block`）：`arg` 是服务端按
                    // **已交付事件**算出的差值——即客户端此刻还缺的余量，所以追加不会
                    // 与已应用的 `TextDelta` 重复；`text` 非空则是整流全量覆盖（丢/乱序
                    // delta 后、或非追加改写时才会出现）。
                    //
                    // 注意：**不能**写成 `block.text = text.clone()`。`text` 在正常增量
                    // 路径下是空串（`skip_serializing_if` 会让它直接缺席），那样写会在
                    // 每个检查点把已流出的正文清空。
                    if let Some(arg) = arg {
                        block.text.push_str(arg);
                    }
                    if !text.is_empty() {
                        block.text = text.clone();
                    }
                    block.touch();
                } else {
                    missing_block_drops += 1;
                    changed = false;
                }
            }
            E::ToolUpdated { block_id, tool } => {
                let round_num = entry.round_num.unwrap_or(0);
                let round = Self::find_round_mut(turn, round_num);
                let card = ToolCard::from(tool.clone());
                if let Some(block) = Self::find_block_mut(round, block_id) {
                    block.tool = Some(card);
                    block.touch();
                } else {
                    missing_block_drops += 1;
                    changed = false;
                }
            }
            // TODO(M3): stream/bytes_total 是跨仓契约 §5 的进度元数据，消费在 M3 接入。
            E::ToolProgress {
                block_id,
                chunk,
                truncated,
                stream: _,
                bytes_total: _,
            } => {
                let round_num = entry.round_num.unwrap_or(0);
                let round = Self::find_round_mut(turn, round_num);
                if let Some(block) = Self::find_block_mut(round, block_id) {
                    if let Some(tool) = block.tool.as_mut() {
                        let is_bash_stream = matches!(
                            tool.name.as_str(),
                            "bash" | "exec" | "shell" | "pwsh" | "powershell"
                        );
                        let local_truncated = if is_bash_stream {
                            apply_bash_progress(&mut tool.progress, chunk)
                        } else {
                            tool.progress.push_str(chunk);
                            retain_utf8_tail(&mut tool.progress, MAX_PROGRESS_LEN)
                        };
                        tool.progress_truncated |= *truncated || local_truncated;
                        block.touch();
                    } else {
                        missing_block_drops += 1;
                        changed = false;
                    }
                } else {
                    missing_block_drops += 1;
                    changed = false;
                }
            }
            E::BlockSealed { block_id } => {
                let round_num = entry.round_num.unwrap_or(0);
                let round = Self::find_round_mut(turn, round_num);
                if let Some(block) = Self::find_block_mut(round, block_id) {
                    block.state = TimelineBlockState::Sealed;
                    block.touch();
                } else {
                    missing_block_drops += 1;
                    changed = false;
                }
            }
            E::RoundSealed { is_final } => {
                let round_num = entry.round_num.unwrap_or(0);
                let round = Self::find_round_mut(turn, round_num);
                round.sealed = true;
                round.is_final = *is_final;
            }
            E::TurnSealed { state, failure } => {
                turn.state = *state;
                turn.failure = failure.clone();
                turn.sealed = true;
                // D1：封口即丢 reasoning body，聚合计数入回合头（§4.6）。
                discard_sealed_reasoning(turn);
                terminal = Some(TurnTerminal {
                    turn_id: turn_id.to_owned(),
                    state: *state,
                });
                changed = true;
            }
        }
        if missing_block_drops > 0 {
            self.dropped_missing_block += u64::from(missing_block_drops);
        }
        if changed {
            self.bump();
        }
        terminal
    }

    /// 快照整体替换（re-baseline / 打开标签页）。
    pub fn replace_from_page(&mut self, page: &TimelinePage) {
        self.turns = page
            .snapshot
            .turns
            .iter()
            .map(|t| Turn::from_wire(t.clone()))
            .collect();
        self.has_more = page.has_more;
        self.total_turns = page.total_turns;
        self.truncated_before = page.truncated_before;
        self.dropped_turns = page.total_turns.saturating_sub(self.turns.len());
        self.rebaseline_epoch = self.rebaseline_epoch.saturating_add(1);
        self.bump();
    }

    /// 加载更早的回合（滚动上翻分页）。
    pub fn prepend_older(&mut self, page: &TimelinePage) {
        let older: Vec<Turn> = page
            .snapshot
            .turns
            .iter()
            .map(|t| Turn::from_wire(t.clone()))
            .collect();
        if older.is_empty() {
            self.has_more = false;
            // 空页 = 翻到头那一刻。服务端此时仍会带真实总数与 truncated_before，
            // 必须一并接收——「历史到底多长」「是否被裁剪」正是在这一刻才确定，
            // 早先就丢弃会让 UI 永远说不出「还有 N 轮够不到」。
            self.total_turns = self.total_turns.max(page.total_turns);
            self.truncated_before = page.truncated_before;
            self.bump();
            return;
        }
        let last_id = older.last().map(|t| t.turn_id.clone()).unwrap_or_default();
        let mut merged: Vec<Turn> = older;
        for turn in self.turns.drain(..) {
            if turn.turn_id == last_id {
                // 服务端分页含边界回合：用内存中较新的版本（older 尾与现存首重合）。
                merged.pop();
                merged.push(turn);
            } else {
                merged.push(turn);
            }
        }
        self.turns = merged;
        self.has_more = page.has_more;
        self.total_turns = self.total_turns.max(page.total_turns);
        self.truncated_before = page.truncated_before;
        self.bump();
    }

    /// 内存中回合滑动窗口上限（对照 opencode sync 的 messages limit=100 +
    /// 窗口外裁剪）。超出时从最旧一侧丢弃并置 has_more=true——加载更早仍可用
    /// （before_turn 锚点取内存窗口首回合，服务端始终是权威历史）。
    /// 裁剪内存回合窗口至 `max`（**当前无生产调用者**）。
    ///
    /// 保留原因：这是唯一能主动释放 `turns` 的入口，作为极端情况（如 daemon
    /// 未启用 offload 且会话异常长）的兜底手段。
    ///
    /// ⚠ 若重新启用：`dropped_turns` 必须随之递增（已内建），否则回合编号会
    /// 集体前移、渲染缓存全失效（见 `turn_number`）。
    #[allow(dead_code)]
    pub fn cap_turns(&mut self, max: usize) {
        if max > 0 && self.turns.len() > max {
            let drop = self.turns.len() - max;
            self.turns.drain(..drop);
            // 单调计数：回合编号靠它保持稳定（见 `turn_number`）。
            self.dropped_turns += drop;
            self.has_more = true;
            self.bump();
        }
    }

    /// 某回合的**稳定**显示编号（1-based）。
    ///
    /// 不能用 `idx + 1`：`cap_turns` 丢头部后所有编号会集体前移，导致
    /// 缓存键全失效（每回合全量重渲，实测 19/19 → 197ms 悬崖）。
    /// 优先用后端给的全局序号；实时事件不带（实测 100% 缺失）时用
    /// 「已丢弃数 + 窗口内下标」推算——两者都与窗口起点无关。
    pub fn turn_number(&self, idx: usize) -> u64 {
        match self.turns.get(idx).and_then(|t| t.turn_index) {
            Some(gi) => gi + 1,
            None => (self.dropped_turns + idx + 1) as u64,
        }
    }

    /// 会话总回合数（含已从窗口丢弃的）。
    pub fn turn_total(&self) -> u64 {
        self.total_turns.max(self.dropped_turns + self.turns.len()) as u64
    }

    pub fn last_turn_id(&self) -> Option<&str> {
        self.turns.last().map(|t| t.turn_id.as_str())
    }

    /// 窗口内是否还有 running turn（任一；正常至多一个）。
    pub fn is_streaming(&self) -> bool {
        self.turns.iter().any(|t| t.is_streaming())
    }

    /// 窗口内最新的 running turn（跳过已被淘汰的旧 running 幽灵）。
    pub fn running_turn_id(&self) -> Option<&str> {
        self.turns
            .iter()
            .rev()
            .find(|t| t.is_streaming())
            .map(|t| t.turn_id.as_str())
    }

    /// 该 turn 在窗口内的运行态：`Some(true)` 仍在跑，`Some(false)` 已终态，
    /// `None` 表示已滑出窗口（尾部窗口语义下等于"更旧、已被更新 turn 顶掉"）。
    pub fn turn_running(&self, turn_id: &str) -> Option<bool> {
        self.turns
            .iter()
            .find(|t| t.turn_id == turn_id)
            .map(|t| t.is_streaming())
    }

    /// 按 `tool_call_id` 在窗口内找工具卡（v2 permission 交互的面板详情来源）。
    pub fn tool_card(&self, call_id: &str) -> Option<&ToolCard> {
        self.turns.iter().find_map(|turn| {
            turn.rounds.iter().find_map(|round| {
                round
                    .blocks
                    .iter()
                    .filter_map(|block| block.tool.as_ref())
                    .find(|card| card.tool_call_id == call_id)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_client::{TimelineEvent, TimelineToolState, TimelineTurnState};

    fn entry(seq: u64, turn: &str, event: TimelineEvent) -> TimelineEntry {
        TimelineEntry {
            timeline_seq: seq,
            turn_id: turn.to_owned(),
            round_num: Some(0),
            event,
        }
    }

    #[test]
    fn replace_from_page_advances_rebaseline_epoch() {
        let page = TimelinePage {
            schema: "qaqh.Ringing".into(),
            version: 1,
            server_epoch: "ep".into(),
            seed: "s".into(),
            has_more: false,
            total_turns: 0,
            truncated_before: false,
            snapshot: qaqh_client::TimelineSnapshot {
                watermark: 0,
                turns: vec![],
            },
        };
        let mut model = TimelineModel::default();
        let version = model.version;
        model.replace_from_page(&page);
        assert_eq!(model.rebaseline_epoch, 1);
        assert_eq!(model.dropped_turns, 0);
        assert_eq!(model.version, version + 1);
    }

    /// 权威语义：`BlockCheckpoint.arg` 是**增量**，必须**追加**而不是覆盖。
    ///
    /// 这是后端正常路径下发的形态：`arg` 携带「按已交付事件算出的余量」，
    /// `text` 为空并因 `skip_serializing_if` 直接缺席。
    ///
    /// 锁的是一个**差点写错**的地方：旧代码无条件 `block.text = text.clone()`。
    /// 若只把镜像的 `text` 放宽为可缺省（而不改语义），每个检查点都会把已流出
    /// 的正文清空——本仓历史上最忌讳的「真吞字」。
    #[test]
    fn incremental_checkpoint_appends_and_never_wipes() {
        use qaqh_client::TimelineEvent as E;
        let mut m = TimelineModel::default();
        m.apply(&entry(
            1,
            "t1",
            E::TurnOpened {
                user_text: "问".into(),
            },
        ));
        m.apply(&entry(
            2,
            "t1",
            E::BlockOpened {
                block: TimelineBlock {
                    block_id: "b1".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: None,
                },
            },
        ));
        m.apply(&entry(
            3,
            "t1",
            E::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 1,
                delta: "回答".into(),
            },
        ));
        // 增量检查点：arg = 已交付事件之后仍缺的余量，text 缺席。
        m.apply(&entry(
            4,
            "t1",
            E::BlockCheckpoint {
                block_id: "b1".into(),
                arg: Some("开始了".into()),
                text: String::new(),
            },
        ));

        let text = m.turns[0].rounds[0].blocks[0].text.clone();
        assert_eq!(
            text, "回答开始了",
            "增量检查点必须**追加**：既不能丢掉 arg（少文本），也不能用空的 text 覆盖（吞文本）"
        );

        // 后半段：整流形态（text 非空）仍然覆盖，且随后的增量继续追加。
        m.apply(&entry(
            5,
            "t1",
            E::BlockCheckpoint {
                block_id: "b1".into(),
                arg: None,
                text: "重建后的全文".into(),
            },
        ));
        assert_eq!(m.turns[0].rounds[0].blocks[0].text, "重建后的全文");
        m.apply(&entry(
            6,
            "t1",
            E::BlockCheckpoint {
                block_id: "b1".into(),
                arg: Some("！".into()),
                text: String::new(),
            },
        ));
        assert_eq!(m.turns[0].rounds[0].blocks[0].text, "重建后的全文！");
    }

    #[test]
    fn turn_and_text_flow() {
        let mut m = TimelineModel::default();
        m.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "你好".into(),
            },
        ));
        m.apply(&entry(
            2,
            "t1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "b1".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: None,
                },
            },
        ));
        m.apply(&entry(
            3,
            "t1",
            TimelineEvent::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 1,
                delta: "回答".into(),
            },
        ));
        m.apply(&entry(
            4,
            "t1",
            TimelineEvent::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 2,
                delta: "开始".into(),
            },
        ));
        m.apply(&entry(
            5,
            "t1",
            TimelineEvent::BlockCheckpoint {
                block_id: "b1".into(),
                // 全量覆盖形态（`arg` 缺席）——正是这段文本触发整流的情形。
                arg: None,
                text: "回答开始了".into(),
            },
        ));
        m.apply(&entry(
            6,
            "t1",
            TimelineEvent::BlockSealed {
                block_id: "b1".into(),
            },
        ));
        m.apply(&entry(
            7,
            "t1",
            TimelineEvent::RoundSealed { is_final: true },
        ));
        m.apply(&entry(
            8,
            "t1",
            TimelineEvent::TurnSealed {
                state: TimelineTurnState::Completed,
                failure: None,
            },
        ));

        assert_eq!(m.turns.len(), 1);
        let turn = &m.turns[0];
        assert_eq!(turn.state, TimelineTurnState::Completed);
        assert_eq!(turn.rounds[0].blocks[0].text, "回答开始了");
        assert!(m.turns.iter().all(|t| !t.is_streaming()));
    }

    /// 流式文本块的构造夹具（`block_order` 恒 0，够用）。
    fn text_block(id: &str) -> TimelineBlock {
        TimelineBlock {
            block_id: id.into(),
            block_order: 0,
            kind: TimelineBlockKind::Text,
            state: TimelineBlockState::Open,
            text: String::new(),
            tool: None,
        }
    }

    /// **T-07 回归**：已 sealed 的回合收到新 `TurnOpened` 必须**原地 reopen**。
    ///
    /// 后端语义（`qaqh-runtime/src/timeline.rs:208-244`）：daemon 重启后 worker 的
    /// turn 计数可能滞后，于是把**已 sealed** 的 turn_id 复用给新输入；后端此时
    /// 原地重置该回合，而非报 `DuplicateTurn`。
    ///
    /// 旧行为是直接 no-op（只有 `find_turn_mut().is_none()` 才建回合），后果是
    /// 上一轮的 rounds 残留、state 停在终态 → 新旧内容混在同一回合，且
    /// `running_turn_id()` 不认它（流式指示与收口全部错位）。
    #[test]
    fn sealed_turn_is_reopened_in_place() {
        let mut m = TimelineModel::default();
        m.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "第一轮".into(),
            },
        ));
        m.apply(&entry(
            2,
            "t1",
            TimelineEvent::BlockOpened {
                block: text_block("b1"),
            },
        ));
        m.apply(&entry(
            3,
            "t1",
            TimelineEvent::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 0,
                delta: "旧内容".into(),
            },
        ));
        // 上一轮还有第二个块：它不会被新尝试复用，是「旧内容残留」的探针
        // （只在 reopen 真正清空 rounds 时才会消失）。
        m.apply(&entry(
            4,
            "t1",
            TimelineEvent::BlockOpened {
                block: text_block("b2"),
            },
        ));
        m.apply(&entry(
            5,
            "t1",
            TimelineEvent::TextDelta {
                block_id: "b2".into(),
                fragment_seq: 0,
                delta: "上一轮的残留".into(),
            },
        ));
        m.apply(&entry(
            6,
            "t1",
            TimelineEvent::TurnSealed {
                state: TimelineTurnState::Completed,
                failure: None,
            },
        ));
        assert!(m.turns[0].sealed, "TurnSealed 必须置 sealed");
        let version_before = m.version;

        // 复用同一 turn_id 的新输入
        m.apply(&entry(
            7,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "第二轮".into(),
            },
        ));

        assert_eq!(m.turns.len(), 1, "必须原地 reopen，不得新建回合");
        let turn = &m.turns[0];
        assert_eq!(turn.user_text, "第二轮", "user_text 必须换新");
        assert_eq!(turn.state, TimelineTurnState::Running, "必须回到 Running");
        assert!(!turn.sealed, "reopen 后不再是 sealed");
        assert!(turn.rounds.is_empty(), "上一轮 rounds 必须清空");
        assert!(
            m.version > version_before,
            "内容变化必须 bump，否则渲染缓存不失效"
        );

        // 端到端可见性：新尝试复用 b1，旧块 b2 不得再出现。
        m.apply(&entry(
            8,
            "t1",
            TimelineEvent::BlockOpened {
                block: text_block("b1"),
            },
        ));
        m.apply(&entry(
            9,
            "t1",
            TimelineEvent::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 0,
                delta: "新首行".into(),
            },
        ));
        let blocks = &m.turns[0].rounds[0].blocks;
        assert_eq!(blocks.len(), 1, "旧块不得残留在同一回合里");
        assert_eq!(blocks[0].block_id, "b1");
        assert_eq!(blocks[0].text, "新首行");
    }

    /// 运行中的重复 `TurnOpened` **不是** reopen：后端对该情形返回
    /// `DuplicateTurn`，能到达 reducer 的只有快照/回放的幂等重投，不得改内容。
    ///
    /// 与 [`sealed_turn_is_reopened_in_place`] 构成一对：`sealed` 是唯一判据。
    #[test]
    fn running_turn_duplicate_opened_keeps_content() {
        let mut m = TimelineModel::default();
        m.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "第一轮".into(),
            },
        ));
        m.apply(&entry(
            2,
            "t1",
            TimelineEvent::BlockOpened {
                block: text_block("b1"),
            },
        ));
        let version_before = m.version;

        m.apply(&entry(
            3,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "重投".into(),
            },
        ));

        let turn = &m.turns[0];
        assert_eq!(turn.user_text, "第一轮", "运行中的重复不得改 user_text");
        assert_eq!(turn.rounds.len(), 1, "运行中的重复不得清 rounds");
        assert_eq!(m.version, version_before, "运行中的重复必须是 no-op");
    }

    #[test]
    fn duplicate_and_replayed_entries_are_idempotent() {
        let mut m = TimelineModel::default();
        m.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "hi".into(),
            },
        ));
        m.apply(&entry(
            2,
            "t1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "b1".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: None,
                },
            },
        ));
        let v = m.version;
        // 重复 TurnOpened → no-op
        m.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "hi".into(),
            },
        ));
        assert_eq!(m.version, v);
        // 重复 fragment → 丢弃
        m.apply(&entry(
            2,
            "t1",
            TimelineEvent::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 1,
                delta: "a".into(),
            },
        ));
        let v2 = m.version;
        m.apply(&entry(
            2,
            "t1",
            TimelineEvent::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 1,
                delta: "a".into(),
            },
        ));
        assert_eq!(m.version, v2);
    }

    #[test]
    fn tool_lifecycle_and_progress() {
        let mut m = TimelineModel::default();
        m.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "run".into(),
            },
        ));
        m.apply(&entry(
            2,
            "t1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "b2".into(),
                    block_order: 1,
                    kind: TimelineBlockKind::Tool,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: Some(TimelineTool {
                        display: None,
                        progress_bytes_total: 0,
                        progress_stream: None,
                        tool_call_id: "c1".into(),
                        name: "exec".into(),
                        state: TimelineToolState::Prepared,
                        summary: None,
                        args_json: Some("{}".into()),
                        output: None,
                        diff: None,
                        progress: String::new(),
                        progress_truncated: false,
                        failure: None,
                        permission: None,
                    }),
                },
            },
        ));
        m.apply(&entry(
            3,
            "t1",
            TimelineEvent::ToolProgress {
                block_id: "b2".into(),
                chunk: "out1\n".into(),
                truncated: false,
                stream: None,
                bytes_total: 0,
            },
        ));
        m.apply(&entry(
            4,
            "t1",
            TimelineEvent::ToolProgress {
                block_id: "b2".into(),
                chunk: "out2\n".into(),
                truncated: false,
                stream: None,
                bytes_total: 0,
            },
        ));
        m.apply(&entry(
            5,
            "t1",
            TimelineEvent::TurnSealed {
                state: TimelineTurnState::Completed,
                failure: None,
            },
        ));

        let tool = m.turns[0].rounds[0].blocks[0].tool.as_ref().unwrap();
        assert_eq!(tool.state, TimelineToolState::Prepared);
        assert_eq!(tool.progress, "out1\nout2\n");
    }

    #[test]
    fn cap_turns_keeps_recent_window() {
        let mut m = TimelineModel::default();
        for i in 1..=10 {
            m.apply(&entry(
                i,
                &format!("t{i}"),
                TimelineEvent::TurnOpened {
                    user_text: format!("n{i}"),
                },
            ));
        }
        assert_eq!(m.turns.len(), 10);
        m.cap_turns(4);
        assert_eq!(m.turns.len(), 4);
        assert_eq!(m.turns[0].turn_id, "t7", "保留最近的窗口");
        assert_eq!(m.turns[3].turn_id, "t10");
        assert!(m.has_more, "窗口外的更早回合标记可加载");
        // 上限内调用是 no-op。
        let v = m.version;
        m.cap_turns(4);
        assert_eq!(m.version, v);
    }

    /// T-08：翻到窗口开头（服务端回空页）且服务端声明「物化窗口覆盖不到历史
    /// 开头」时，模型必须同时记住两件事——「没有更多可交付」与「历史并不止于此」。
    /// 只记前者，UI 就既不翻不动也不说明，用户只能反复按 PgUp 干等一个永远不来的页。
    ///
    /// 破坏验证：把 `truncated_before` 的赋值去掉 → 本测试红。
    #[test]
    fn empty_page_at_the_wall_records_truncation() {
        let mut m = TimelineModel::default();
        m.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "n1".into(),
            },
        ));
        // 200 轮的会话，timeline 重建后只物化到 t1..t40；客户端一路 PgUp 到开头。
        let wall = TimelinePage {
            schema: "qaqh.Ringing".into(),
            version: 1,
            server_epoch: "ep".into(),
            seed: "s".into(),
            has_more: false,
            total_turns: 200,
            truncated_before: true,
            snapshot: qaqh_client::TimelineSnapshot {
                watermark: 0,
                turns: vec![],
            },
        };
        m.prepend_older(&wall);
        assert!(!m.has_more, "空页 ⇒ 没有可再翻的页");
        assert!(m.truncated_before, "服务端声明被裁剪 ⇒ 模型必须记住");
        assert_eq!(
            m.total_turns, 200,
            "总数是会话真实回合数，不是本窗口大小——否则说不出「还有多少轮够不到」"
        );
    }

    /// 反向闸：窗口完整时不得谎报「被裁剪」，否则每个正常会话都会挂一条警告。
    #[test]
    fn complete_window_reports_no_truncation() {
        let mut m = TimelineModel::default();
        m.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "n1".into(),
            },
        ));
        let wall = TimelinePage {
            schema: "qaqh.Ringing".into(),
            version: 1,
            server_epoch: "ep".into(),
            seed: "s".into(),
            has_more: false,
            total_turns: 1,
            truncated_before: false,
            snapshot: qaqh_client::TimelineSnapshot {
                watermark: 0,
                turns: vec![],
            },
        };
        m.prepend_older(&wall);
        assert!(!m.truncated_before, "窗口完整 ⇒ 不得报警告");
        assert_eq!(m.total_turns, 1);
    }

    #[test]
    fn prepend_older_merges_boundary() {
        let mut m = TimelineModel::default();
        for i in 3..=4 {
            m.apply(&entry(
                i as u64,
                &format!("t{i}"),
                TimelineEvent::TurnOpened {
                    user_text: format!("n{i}"),
                },
            ));
        }
        // 服务端返回 t1..t3，其中 t3 为边界重复。
        let page = TimelinePage {
            schema: "qaqh.Ringing".into(),
            version: 1,
            server_epoch: "ep".into(),
            seed: "s".into(),
            has_more: false,
            total_turns: 4,
            truncated_before: false,
            snapshot: qaqh_client::TimelineSnapshot {
                watermark: 3,
                turns: (1..=3)
                    .map(|i| qaqh_client::TimelineTurn {
                        turn_index: None,
                        turn_id: format!("t{i}"),
                        created_seq: i as u64,
                        user_text: format!("n{i}"),
                        sealed: true,
                        offloaded: false,
                        state: TimelineTurnState::Completed,
                        failure: None,
                        rounds: vec![],
                    })
                    .collect(),
            },
        };
        m.prepend_older(&page);
        assert_eq!(m.turns.len(), 4);
        assert_eq!(m.turns[0].turn_id, "t1");
        assert_eq!(m.turns[2].turn_id, "t3");
        assert_eq!(m.turns[3].turn_id, "t4");
    }

    /// BUG-2026-09-15-05：翻页游标取自 `turn_index`（全局回合序号），**不是**
    /// `turn_id`——后者会被 worker 复用，当不了稳定游标。这条钉住「本页最旧那个
    /// 回合带着序号进来了」，因为 `load_older` 的游标正是从它取的。
    #[test]
    fn prepend_older_carries_global_turn_index_for_the_next_cursor() {
        let mut m = TimelineModel::default();
        // 常驻窗口是重建后的最后 40 轮：t21..t60（id 由全局序号派生）
        m.apply(&entry(
            60,
            "t60",
            TimelineEvent::TurnOpened {
                user_text: "n60".into(),
            },
        ));
        // 深翻页回来的那页：全局序号 11..=21（id 是 t12..t22）
        let page = TimelinePage {
            schema: "qaqh.Ringing".into(),
            version: 1,
            server_epoch: "ep".into(),
            seed: "s".into(),
            has_more: true,
            total_turns: 60,
            truncated_before: false,
            snapshot: qaqh_client::TimelineSnapshot {
                watermark: 3,
                turns: (11..=21u64)
                    .map(|i| qaqh_client::TimelineTurn {
                        turn_index: Some(i),
                        turn_id: format!("t{}", i + 1),
                        created_seq: i,
                        user_text: format!("n{}", i + 1),
                        sealed: true,
                        offloaded: false,
                        state: TimelineTurnState::Completed,
                        failure: None,
                        rounds: vec![],
                    })
                    .collect(),
            },
        };
        m.prepend_older(&page);
        assert_eq!(
            m.turns.first().and_then(|t| t.turn_index),
            Some(11),
            "下一页的游标 = 本页最旧回合的全局序号"
        );
        // 反向闸：实时追加的回合不带序号——拿它当游标会翻错页，故 `load_older`
        // 必须 `and_then`（那条路径不在这里测，但这条断言钉住了「None 是可能的」）。
        assert_eq!(m.turns.last().and_then(|t| t.turn_index), None);
    }

    #[test]
    fn strip_ansi_removes_csi_and_osc() {
        let s = "\x1b[31mred\x1b[0m plain \x1b[2K\x1b[1A";
        assert_eq!(strip_ansi_escapes(s), "red plain ");
        let s2 = "a\x1b]0;title\x07b";
        assert_eq!(strip_ansi_escapes(s2), "ab");
    }

    #[test]
    fn bash_progress_simple_line_append() {
        let mut buf = String::new();
        apply_bash_progress(&mut buf, "hello\n");
        apply_bash_progress(&mut buf, "world\n");
        assert_eq!(buf, "hello\nworld\n");
    }

    #[test]
    fn bash_progress_carriage_return_overwrites_line() {
        let mut buf = String::new();
        apply_bash_progress(&mut buf, "Downloading 10%\r");
        apply_bash_progress(&mut buf, "Downloading 50%\r");
        apply_bash_progress(&mut buf, "Downloading 100%\n");
        assert_eq!(buf, "Downloading 100%\n");
    }

    #[test]
    fn bash_progress_apt_like_simulation() {
        // debian apt 无 tty 时多为行日志，tty 时 \r 覆盖；此处模拟后者
        let chunks = [
            "Get:1 http://deb.debian.org stable InRelease [100 kB]\n",
            "0% [Working]\r",
            "5% [Waiting]\r",
            "100% [Done]\n",
            "Fetched 10 MB in 1s\n",
        ];
        let out = normalize_progress_history(&chunks);
        assert!(out.contains("Get:1"), "首行保留");
        assert!(out.contains("100% [Done]"), "最后覆盖行保留");
        assert!(!out.contains("0% [Working]"), "被覆盖的中间行不应保留");
        assert!(!out.contains("\r"), "不应透出 \r");
        assert!(!out.contains("\x1b"), "ANSI 已剥离");
    }

    #[test]
    fn bash_progress_ansi_stripped_from_chunk() {
        let mut buf = String::new();
        apply_bash_progress(&mut buf, "\x1b[32mOK\x1b[0m\n");
        assert_eq!(buf, "OK\n");
        let mut buf2 = String::new();
        apply_bash_progress(&mut buf2, "\x1b[2K\r\x1b[33mprogress 50%\x1b[0m\r");
        // \r 覆写语义需保留最后非空
        assert_eq!(buf2, "progress 50%");
    }

    #[test]
    fn bash_progress_crlf_is_newline() {
        let mut buf = String::new();
        apply_bash_progress(&mut buf, "line1\r\nline2\r\n");
        assert_eq!(buf, "line1\nline2\n");
    }

    #[test]
    fn bash_progress_multiple_cr_in_one_chunk() {
        let mut buf = String::new();
        apply_bash_progress(&mut buf, "a\rb\rc\n");
        assert_eq!(buf, "c\n");
        let mut buf2 = String::new();
        apply_bash_progress(&mut buf2, "first\nsecond\rOVER\n");
        assert_eq!(buf2, "first\nOVER\n");
    }

    #[test]
    fn bash_progress_tail_truncation_keeps_utf8() {
        let mut buf = String::new();
        let chunk = "x".repeat(9000) + "\n";
        apply_bash_progress(&mut buf, &chunk);
        assert!(buf.len() <= 8192, "超长应截尾 {}", buf.len());
        assert!(buf.is_char_boundary(0));
        assert!(buf.ends_with("\n") || !buf.is_empty());
        // 中文：3 bytes/char，8192 字节预算约 2730 字，6144 保留约 2048 字
        let mut buf2 = String::new();
        let zh = "中".repeat(5000);
        apply_bash_progress(&mut buf2, &zh);
        assert!(buf2.is_char_boundary(0));
        // 15000 bytes -> 截到 6144 字节尾 -> 2048 个中文
        assert_eq!(buf2.chars().count(), 2048);
        assert!(buf2.len() <= 8192);
    }

    #[test]
    fn non_bash_progress_is_bounded_and_marks_truncation() {
        let mut m = TimelineModel::default();
        m.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "hi".into(),
            },
        ));
        m.apply(&entry(
            2,
            "t1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "b1".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Tool,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: Some(TimelineTool {
                        display: None,
                        progress_bytes_total: 0,
                        progress_stream: None,
                        tool_call_id: "c1".into(),
                        name: "read".into(),
                        state: TimelineToolState::Running,
                        summary: None,
                        args_json: None,
                        output: None,
                        diff: None,
                        progress: String::new(),
                        progress_truncated: false,
                        failure: None,
                        permission: None,
                    }),
                },
            },
        ));
        m.apply(&entry(
            3,
            "t1",
            TimelineEvent::ToolProgress {
                block_id: "b1".into(),
                chunk: format!("{}{}", "x".repeat(9000), "tail"),
                truncated: false,
                stream: None,
                bytes_total: 0,
            },
        ));

        let tool = m.turns[0].rounds[0].blocks[0].tool.as_ref().unwrap();
        assert!(tool.progress.len() <= MAX_PROGRESS_LEN);
        assert!(tool.progress.ends_with("tail"));
        assert!(tool.progress_truncated);
    }

    #[test]
    fn tool_progress_event_truncation_is_propagated() {
        let mut m = TimelineModel::default();
        m.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "hi".into(),
            },
        ));
        m.apply(&entry(
            2,
            "t1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "b1".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Tool,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: Some(TimelineTool {
                        display: None,
                        progress_bytes_total: 0,
                        progress_stream: None,
                        tool_call_id: "c1".into(),
                        name: "exec".into(),
                        state: TimelineToolState::Running,
                        summary: None,
                        args_json: None,
                        output: None,
                        diff: None,
                        progress: String::new(),
                        progress_truncated: false,
                        failure: None,
                        permission: None,
                    }),
                },
            },
        ));
        m.apply(&entry(
            3,
            "t1",
            TimelineEvent::ToolProgress {
                block_id: "b1".into(),
                chunk: "tail".into(),
                truncated: true,
                stream: None,
                bytes_total: 0,
            },
        ));

        let tool = m.turns[0].rounds[0].blocks[0].tool.as_ref().unwrap();
        assert_eq!(tool.progress, "tail");
        assert!(tool.progress_truncated);
    }

    #[test]
    fn legacy_timeline_json_defaults_new_bounded_fields() {
        let tool: TimelineTool = serde_json::from_value(serde_json::json!({
            "tool_call_id": "c1",
            "name": "exec",
            "state": "running",
            "progress": "tail"
        }))
        .unwrap();
        assert!(!tool.progress_truncated);

        let event: TimelineEvent = serde_json::from_value(serde_json::json!({
            "type": "tool_progress",
            "block_id": "b1",
            "chunk": "tail"
        }))
        .unwrap();
        assert!(matches!(
            event,
            TimelineEvent::ToolProgress {
                truncated: false,
                ..
            }
        ));

        let turn: TimelineTurn = serde_json::from_value(serde_json::json!({
            "turn_id": "t1",
            "created_seq": 1,
            "user_text": "hi",
            "sealed": false,
            "state": "running",
            "rounds": []
        }))
        .unwrap();
        assert!(!turn.offloaded);
    }

    #[test]
    fn bash_progress_via_timeline_bash_vs_other_tool() {
        // bash 工具应走 apply_bash_progress，普通工具保持 push_str
        let mut m = TimelineModel::default();
        m.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "hi".into(),
            },
        ));
        for (bid, name) in [("b_bash", "bash"), ("b_read", "read")] {
            m.apply(&entry(
                2,
                "t1",
                TimelineEvent::BlockOpened {
                    block: TimelineBlock {
                        block_id: bid.into(),
                        block_order: if name == "bash" { 0 } else { 1 },
                        kind: TimelineBlockKind::Tool,
                        state: TimelineBlockState::Open,
                        text: String::new(),
                        tool: Some(TimelineTool {
                            display: None,
                            progress_bytes_total: 0,
                            progress_stream: None,
                            tool_call_id: format!("c_{name}"),
                            name: name.into(),
                            state: TimelineToolState::Running,
                            summary: None,
                            args_json: None,
                            output: None,
                            diff: None,
                            progress: String::new(),
                            progress_truncated: false,
                            failure: None,
                            permission: None,
                        }),
                    },
                },
            ));
        }
        m.apply(&entry(
            3,
            "t1",
            TimelineEvent::ToolProgress {
                block_id: "b_bash".into(),
                chunk: "a\rb\n".into(),
                truncated: false,
                stream: None,
                bytes_total: 0,
            },
        ));
        m.apply(&entry(
            4,
            "t1",
            TimelineEvent::ToolProgress {
                block_id: "b_read".into(),
                chunk: "a\rb\n".into(),
                truncated: false,
                stream: None,
                bytes_total: 0,
            },
        ));
        let bash_progress = m.turns[0].rounds[0]
            .blocks
            .iter()
            .find(|b| b.block_id == "b_bash")
            .unwrap()
            .tool
            .as_ref()
            .unwrap()
            .progress
            .clone();
        let read_progress = m.turns[0].rounds[0]
            .blocks
            .iter()
            .find(|b| b.block_id == "b_read")
            .unwrap()
            .tool
            .as_ref()
            .unwrap()
            .progress
            .clone();
        assert_eq!(bash_progress, "b\n", "bash 需 \r 覆写");
        assert_eq!(read_progress, "a\rb\n", "非 bash 保持原文");
    }

    #[test]
    fn snapshot_progress_normalized_for_bash() {
        let page = TimelinePage {
            schema: "qaqh.Ringing".into(),
            version: 1,
            server_epoch: "ep".into(),
            seed: "s".into(),
            has_more: false,
            total_turns: 1,
            truncated_before: false,
            snapshot: qaqh_client::TimelineSnapshot {
                watermark: 1,
                turns: vec![qaqh_client::TimelineTurn {
                    turn_index: None,
                    turn_id: "t1".into(),
                    created_seq: 1,
                    user_text: "hi".into(),
                    sealed: true,
                    offloaded: false,
                    state: TimelineTurnState::Completed,
                    failure: None,
                    rounds: vec![qaqh_client::TimelineRound {
                        round_num: 0,
                        sealed: true,
                        is_final: true,
                        blocks: vec![TimelineBlock {
                            block_id: "b1".into(),
                            block_order: 0,
                            kind: TimelineBlockKind::Tool,
                            state: TimelineBlockState::Sealed,
                            text: String::new(),
                            tool: Some(TimelineTool {
                                display: None,
                                progress_bytes_total: 0,
                                progress_stream: None,
                                tool_call_id: "c1".into(),
                                name: "bash".into(),
                                state: TimelineToolState::Succeeded,
                                summary: None,
                                args_json: None,
                                output: None,
                                diff: None,
                                progress: "\x1b[31m0%\r100%\n".into(),
                                progress_truncated: false,
                                failure: None,
                                permission: None,
                            }),
                        }],
                    }],
                }],
            },
        };
        let mut m = TimelineModel::default();
        m.replace_from_page(&page);
        let prog = m.turns[0].rounds[0].blocks[0]
            .tool
            .as_ref()
            .unwrap()
            .progress
            .clone();
        assert_eq!(prog, "100%\n");
        assert!(!prog.contains("\x1b"));
        assert!(!prog.contains("\r"));
    }

    #[test]
    fn first_fragment_seq_zero_is_applied() {
        // 回归：daemon 每块首分片 fragment_seq=0（open_block 空文本开块），
        // 曾因 `0 > last_fragment(0)` 恒假被静默丢弃 —— 流式首行吞字，
        // 只能等 BlockCheckpoint 自愈。fresh 块必须接受 seq=0。
        let mut m = TimelineModel::default();
        m.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "hi".into(),
            },
        ));
        m.apply(&entry(
            2,
            "t1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "b1".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: None,
                },
            },
        ));
        m.apply(&entry(
            3,
            "t1",
            TimelineEvent::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 0,
                delta: "首段".into(),
            },
        ));
        m.apply(&entry(
            4,
            "t1",
            TimelineEvent::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 1,
                delta: "正文".into(),
            },
        ));
        assert_eq!(m.turns[0].rounds[0].blocks[0].text, "首段正文");
    }

    #[test]
    fn replayed_first_fragment_is_idempotent() {
        // seq=0 应用后再次回放：text 非空 → 不再 fresh，幂等丢弃不重复追加。
        let mut m = TimelineModel::default();
        m.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "hi".into(),
            },
        ));
        m.apply(&entry(
            2,
            "t1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "b1".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: None,
                },
            },
        ));
        let first = entry(
            3,
            "t1",
            TimelineEvent::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 0,
                delta: "首段".into(),
            },
        );
        m.apply(&first);
        let v = m.version;
        m.apply(&first);
        assert_eq!(m.version, v, "回放不得 bump version");
        assert_eq!(m.turns[0].rounds[0].blocks[0].text, "首段");
    }

    #[test]
    fn snapshot_text_block_rejects_replayed_first_fragment() {
        // re-baseline：快照块带累计文本（last_fragment=0），回放 seq=0
        // 不得重复追加；后续活跃 delta（seq 更大）正常追加。
        let page = TimelinePage {
            schema: "qaqh.Ringing".into(),
            version: 1,
            server_epoch: "ep".into(),
            seed: "s".into(),
            has_more: false,
            total_turns: 1,
            truncated_before: false,
            snapshot: qaqh_client::TimelineSnapshot {
                watermark: 10,
                turns: vec![qaqh_client::TimelineTurn {
                    turn_index: None,
                    turn_id: "t1".into(),
                    created_seq: 1,
                    user_text: "hi".into(),
                    sealed: false,
                    offloaded: false,
                    state: TimelineTurnState::Running,
                    failure: None,
                    rounds: vec![qaqh_client::TimelineRound {
                        round_num: 0,
                        sealed: false,
                        is_final: false,
                        blocks: vec![TimelineBlock {
                            block_id: "b1".into(),
                            block_order: 0,
                            kind: TimelineBlockKind::Text,
                            state: TimelineBlockState::Open,
                            text: "快照内容".into(),
                            tool: None,
                        }],
                    }],
                }],
            },
        };
        let mut m = TimelineModel::default();
        m.replace_from_page(&page);
        m.apply(&entry(
            11,
            "t1",
            TimelineEvent::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 0,
                delta: "回放".into(),
            },
        ));
        assert_eq!(m.turns[0].rounds[0].blocks[0].text, "快照内容");
        m.apply(&entry(
            12,
            "t1",
            TimelineEvent::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 3,
                delta: "+增量".into(),
            },
        ));
        assert_eq!(m.turns[0].rounds[0].blocks[0].text, "快照内容+增量");
    }

    /// B1 可观测回归：只有契约异常（block/turn 缺失）计入 dropped_*，
    /// 设计内幂等重放（旧 fragment_seq）不计入；丢弃不 bump version。
    #[test]
    fn dropped_counters_signal_contract_anomalies_only() {
        let mut m = TimelineModel::default();
        assert_eq!(m.dropped_summary(), None, "恒零必须零噪声");

        // 引用缺失 turn 的事件：计入 missing_turn，不 bump version。
        m.apply(&entry(
            1,
            "ghost",
            TimelineEvent::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 0,
                delta: "x".into(),
            },
        ));
        assert_eq!(m.dropped_missing_turn, 1);
        assert_eq!(m.dropped_missing_block, 0);
        assert_eq!(m.version, 0, "丢弃不触发渲染缓存失效");
        assert!(m.dropped_summary().is_some());

        // 引用缺失 block 的事件：计入 missing_block。
        m.apply(&entry(
            2,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "问".into(),
            },
        ));
        m.apply(&entry(
            3,
            "t1",
            TimelineEvent::TextDelta {
                block_id: "b-missing".into(),
                fragment_seq: 0,
                delta: "y".into(),
            },
        ));
        assert_eq!(m.dropped_missing_block, 1);

        // 设计内幂等重放（旧 fragment_seq）不计入丢弃。
        m.apply(&entry(
            4,
            "t1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "b1".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: None,
                },
            },
        ));
        m.apply(&entry(
            5,
            "t1",
            TimelineEvent::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 0,
                delta: "首".into(),
            },
        ));
        m.apply(&entry(
            6,
            "t1",
            TimelineEvent::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 0,
                delta: "重复".into(),
            },
        ));
        assert_eq!(m.dropped_missing_block, 1, "重放不计入契约异常");
        assert_eq!(m.turns[0].rounds[0].blocks[0].text, "首");
    }

    #[test]
    fn turn_sealed_entry_reports_terminal_and_window_queries() {
        let mut m = TimelineModel::default();
        m.apply(&entry(
            1,
            "t1",
            TimelineEvent::TurnOpened {
                user_text: "q".into(),
            },
        ));
        assert_eq!(m.running_turn_id(), Some("t1"));
        assert_eq!(m.turn_running("t1"), Some(true));
        assert_eq!(m.turn_running("t404"), None);
        assert!(m.is_streaming());

        let terminal = m.apply(&entry(
            2,
            "t1",
            TimelineEvent::TurnSealed {
                state: TimelineTurnState::Completed,
                failure: None,
            },
        ));
        assert_eq!(
            terminal,
            Some(TurnTerminal {
                turn_id: "t1".into(),
                state: TimelineTurnState::Completed,
            })
        );
        assert!(!m.is_streaming());
        assert_eq!(m.running_turn_id(), None);
        assert_eq!(m.turn_running("t1"), Some(false));
    }

    #[test]
    fn missing_turn_entry_has_no_terminal() {
        // 快照窗口外的迟到条目：丢弃 + 计数，且不得伪造终态信号。
        let mut m = TimelineModel::default();
        let terminal = m.apply(&entry(
            1,
            "t404",
            TimelineEvent::TurnSealed {
                state: TimelineTurnState::Failed,
                failure: None,
            },
        ));
        assert!(terminal.is_none());
        assert_eq!(m.dropped_missing_turn, 1);
    }

    // ── D1（M2）：seal 丢 reasoning body，聚合计数入回合头 ────

    fn reasoning_turn_model() -> TimelineModel {
        use qaqh_client::TimelineEvent as E;
        let mut m = TimelineModel::default();
        m.apply(&entry(
            1,
            "t1",
            E::TurnOpened {
                user_text: "问".into(),
            },
        ));
        m.apply(&entry(
            2,
            "t1",
            E::BlockOpened {
                block: TimelineBlock {
                    block_id: "b1".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Reasoning,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: None,
                },
            },
        ));
        m.apply(&entry(
            3,
            "t1",
            E::TextDelta {
                block_id: "b1".into(),
                fragment_seq: 1,
                delta: "第一段\n第二行".into(),
            },
        ));
        m
    }

    #[test]
    fn turn_seal_discards_reasoning_body_and_counts() {
        use qaqh_client::{TimelineEvent as E, TimelineTurnState};
        let mut m = reasoning_turn_model();
        m.apply(&entry(
            4,
            "t1",
            E::BlockSealed {
                block_id: "b1".into(),
            },
        ));
        m.apply(&entry(
            5,
            "t1",
            E::TurnSealed {
                state: TimelineTurnState::Completed,
                failure: None,
            },
        ));
        let t = &m.turns[0];
        assert_eq!(t.thinking.segments, 1);
        assert_eq!(t.thinking.lines, 2);
        assert_eq!(t.rounds[0].blocks[0].text, "", "body 必须已丢弃");
    }

    #[test]
    fn rebaseline_replay_discards_idempotently() {
        use qaqh_client::TimelineTurnState;
        // 分页/re-baseline 重放：sealed reasoning 经 from_wire 同样丢 body 只计数。
        let wire_turn = TimelineTurn {
            turn_index: Some(1),
            turn_id: "t1".into(),
            created_seq: 1,
            user_text: "问".into(),
            sealed: true,
            offloaded: false,
            state: TimelineTurnState::Completed,
            failure: None,
            rounds: vec![qaqh_client::TimelineRound {
                round_num: 0,
                sealed: true,
                is_final: true,
                blocks: vec![TimelineBlock {
                    block_id: "b1".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Reasoning,
                    state: TimelineBlockState::Sealed,
                    text: "重放的思考\n两行".into(),
                    tool: None,
                }],
            }],
        };
        let mut m = TimelineModel::default();
        m.replace_from_page(&TimelinePage {
            schema: "qaqh.Ringing".into(),
            version: 1,
            server_epoch: "ep".into(),
            seed: "s".into(),
            has_more: false,
            total_turns: 1,
            truncated_before: false,
            snapshot: qaqh_client::TimelineSnapshot {
                watermark: 1,
                turns: vec![wire_turn],
            },
        });
        let t = &m.turns[0];
        assert_eq!(t.thinking.segments, 1);
        assert_eq!(t.thinking.lines, 2);
        assert_eq!(t.rounds[0].blocks[0].text, "");
        // 幂等：对已丢弃的块重复 discard 是 no-op。
        super::discard_sealed_reasoning(&mut m.turns[0]);
        assert_eq!(m.turns[0].thinking.segments, 1);
        assert_eq!(m.turns[0].thinking.lines, 2);
    }

    #[test]
    fn reopen_resets_thinking_stats() {
        use qaqh_client::{TimelineEvent as E, TimelineTurnState};
        let mut m = reasoning_turn_model();
        m.apply(&entry(
            4,
            "t1",
            E::BlockSealed {
                block_id: "b1".into(),
            },
        ));
        m.apply(&entry(
            5,
            "t1",
            E::TurnSealed {
                state: TimelineTurnState::Completed,
                failure: None,
            },
        ));
        assert_eq!(m.turns[0].thinking.segments, 1);
        // 原地 reopen：rounds 清空 + stats 归零。
        m.apply(&entry(
            6,
            "t1",
            E::TurnOpened {
                user_text: "新问题".into(),
            },
        ));
        assert_eq!(m.turns[0].thinking, Default::default());
        assert!(m.turns[0].rounds.is_empty());
    }

    // ── 跨仓 wire fixture（契约 §8.3：后端 wire JSON → 前端模型/渲染） ──
    // 每工具族最小集：成功 / 失败 / 截断 / path / MCP fallback / H16 旧 JSON /
    // 未知变体容忍。JSON 形状以契约 §3.3 为准（后端 qaqh-domain 同源类型）。

    fn wire_tool(raw: &str) -> ToolCard {
        let tool: qaqh_client::TimelineTool =
            serde_json::from_str(raw).expect("wire fixture 必须可解析");
        ToolCard::from(tool)
    }

    #[test]
    fn wire_fixture_exec_success_with_progress() {
        let raw = r#"{
            "tool_call_id": "call-1",
            "name": "exec",
            "state": "succeeded",
            "summary": "cargo test \u00b7 2.3s",
            "args_json": "{\"command\":\"cargo test\"}",
            "progress_bytes_total": 12600,
            "progress_stream": "stdout",
            "display": {
                "summary": "cargo test \u00b7 2.3s",
                "header": {"kind": "shell", "command": "cargo test"},
                "body": {"kind": "shell", "output": "test result: ok", "exit_code": 0, "truncated": false},
                "metrics": {"elapsed_ms": 2300, "output_bytes": 4096, "retry_count": 0, "user_initiated": true}
            }
        }"#;
        let card = wire_tool(raw);
        assert_eq!(card.progress_bytes_total, 12600);
        assert_eq!(card.progress_stream.as_deref(), Some("stdout"));
        let d = card.display.as_ref().expect("display");
        assert!(matches!(
            d.header,
            Some(qaqh_client::TimelineToolHeader::Shell { ref command }) if command == "cargo test"
        ));
        assert!(matches!(
            d.body,
            Some(qaqh_client::TimelineToolBody::Shell {
                exit_code: Some(0),
                ..
            })
        ));
        // 渲染断言：标题含完整命令 + metrics 尾注（既有锁的 fixture 形状对齐）。
        let block = Block {
            block_id: "b1".into(),
            block_order: 0,
            kind: TimelineBlockKind::Tool,
            state: TimelineBlockState::Sealed,
            text: String::new(),
            tool: Some(card),
            last_fragment: 0,
            rev: 1,
        };
        let mut sink = crate::app::render_transcript::AnimSink::Bake;
        let lines = crate::app::render_transcript::render_block_lines(&block, 80, &mut sink);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
            .collect::<Vec<_>>()
            .join("|");
        assert!(flat.contains("cargo test"), "exec 标题：{flat}");
        assert!(flat.contains("2.3s"), "metrics 尾注：{flat}");
    }

    #[test]
    fn wire_fixture_exec_failure_and_truncated() {
        let failed = r#"{
            "tool_call_id": "call-2",
            "name": "exec",
            "state": "failed",
            "failure": {"code": "E_TIMEOUT", "message": "超时"},
            "display": {
                "header": {"kind": "shell", "command": "sleep 99"},
                "body": {"kind": "shell", "output": "", "exit_code": 1, "truncated": true}
            }
        }"#;
        let card = wire_tool(failed);
        assert!(matches!(
            card.failure.as_ref().map(|f| f.code.as_str()),
            Some("E_TIMEOUT")
        ));
        assert!(matches!(
            card.display.as_ref().and_then(|d| d.body.as_ref()),
            Some(qaqh_client::TimelineToolBody::Shell {
                truncated: true,
                exit_code: Some(1),
                ..
            })
        ));
    }

    #[test]
    fn wire_fixture_read_path_header() {
        let raw = r#"{
            "tool_call_id": "call-3",
            "name": "read",
            "state": "succeeded",
            "display": {
                "summary": "120 lines",
                "header": {"kind": "path", "path": "src/main.rs", "op": "read"},
                "body": {"kind": "text", "text": "fn main() {}", "truncated": false}
            }
        }"#;
        let card = wire_tool(raw);
        assert!(matches!(
            card.display.as_ref().and_then(|d| d.header.as_ref()),
            Some(qaqh_client::TimelineToolHeader::Path { path, op }) if path == "src/main.rs" && *op == qaqh_client::TimelinePathOp::Read
        ));
    }

    #[test]
    fn wire_fixture_mcp_fallback() {
        let raw = r#"{
            "tool_call_id": "call-4",
            "name": "mcp__github__list_issues",
            "state": "succeeded",
            "display": {
                "summary": "args [repo=owner/name, state=open]",
                "header": {"kind": "other", "label": "mcp__github__list_issues"},
                "body": {"kind": "none"},
                "metrics": {"elapsed_ms": 500, "output_bytes": 0, "retry_count": 0, "effective_tool_name": "list_issues", "user_initiated": false}
            }
        }"#;
        let card = wire_tool(raw);
        let d = card.display.as_ref().expect("display");
        assert!(matches!(
            d.header.as_ref(),
            Some(qaqh_client::TimelineToolHeader::Other { label }) if label == "mcp__github__list_issues"
        ));
        assert!(matches!(d.body, Some(qaqh_client::TimelineToolBody::None)));
        assert_eq!(
            d.metrics
                .as_ref()
                .and_then(|m| m.effective_tool_name.as_deref()),
            Some("list_issues")
        );
    }

    #[test]
    fn wire_fixture_legacy_without_display() {
        // H16：旧 server 快照（无 display / 新字段）必须完整可解析并回退。
        let raw = r#"{
            "tool_call_id": "call-5",
            "name": "bash",
            "state": "succeeded",
            "summary": "ls -la",
            "output": "total 0"
        }"#;
        let card = wire_tool(raw);
        assert!(card.display.is_none());
        assert_eq!(card.progress_bytes_total, 0);
        assert_eq!(card.progress_stream, None);
        assert_eq!(card.output.as_deref(), Some("total 0"));
    }

    #[test]
    fn wire_fixture_unknown_variants_tolerated() {
        // 前向兼容：新 header/body 变体 → Unknown，不丢整块（H7）。
        let raw = r#"{
            "tool_call_id": "call-6",
            "name": "future_tool",
            "state": "succeeded",
            "display": {
                "header": {"kind": "hologram", "beam": 3},
                "body": {"kind": "future_thing", "payload": [1, 2]}
            }
        }"#;
        let card = wire_tool(raw);
        let d = card.display.as_ref().expect("display 不丢");
        assert!(matches!(
            d.header,
            Some(qaqh_client::TimelineToolHeader::Unknown)
        ));
        assert!(matches!(
            d.body,
            Some(qaqh_client::TimelineToolBody::Unknown)
        ));
    }
}
