//! timeline reducer：timeline 事件的唯一消费者（对照 winui `block_transcript.rs`
//! 的语义，修正为幂等可重放）。
//!
//! 快照（bootstrap/分页）整体替换模型；SSE 条目按严格 +1 光标送达（transport
//! 层保证），但 reducer 本身必须对重复应用幂等——undo 后的重取、断点续传的
//! 回放都可能造成重复条目。

use crate::protocol::timeline::{
    TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineFailure,
    TimelinePage, TimelineTool, TimelineToolState, TimelineTurn, TimelineTurnState,
};

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
    pub failure: Option<TimelineFailure>,
    pub permission: Option<crate::protocol::timeline::TimelineToolPermission>,
}

impl From<TimelineTool> for ToolCard {
    fn from(t: TimelineTool) -> Self {
        Self {
            tool_call_id: t.tool_call_id,
            name: t.name,
            state: t.state,
            summary: t.summary,
            args_json: t.args_json,
            output: t.output,
            diff: t.diff,
            progress: t.progress,
            failure: t.failure,
            permission: t.permission,
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
        }
    }

    pub fn is_streaming(&self) -> bool {
        self.state == TimelineBlockState::Open
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Round {
    pub round_num: u32,
    pub sealed: bool,
    pub is_final: bool,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub turn_id: String,
    pub user_text: String,
    pub state: TimelineTurnState,
    pub failure: Option<TimelineFailure>,
    pub rounds: Vec<Round>,
}

impl Turn {
    fn from_wire(t: TimelineTurn) -> Self {
        Self {
            turn_id: t.turn_id,
            user_text: t.user_text,
            state: t.state,
            failure: t.failure,
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
        }
    }

    pub fn is_streaming(&self) -> bool {
        self.state == TimelineTurnState::Running
    }
}

/// 单会话 transcript 模型。`version` 每次变更自增，用于渲染缓存。
#[derive(Debug, Clone, Default)]
pub struct TimelineModel {
    pub turns: Vec<Turn>,
    pub has_more: bool,
    pub total_turns: usize,
    pub version: u64,
}

impl TimelineModel {
    fn bump(&mut self) {
        self.version += 1;
    }

    fn find_turn_mut(&mut self, turn_id: &str) -> Option<&mut Turn> {
        self.turns.iter_mut().find(|t| t.turn_id == turn_id)
    }

    fn find_round_mut(turn: &mut Turn, round_num: u32) -> &mut Round {
        if let Some(idx) = turn.rounds.iter().position(|r| r.round_num == round_num) {
            return &mut turn.rounds[idx];
        }
        turn.rounds.push(Round { round_num, ..Round::default() });
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

    /// 应用一条 timeline 条目（幂等）。
    pub fn apply(&mut self, entry: &TimelineEntry) {
        use crate::protocol::timeline::TimelineEvent as E;
        let turn_id = entry.turn_id.as_str();

        match &entry.event {
            E::TurnOpened { user_text } => {
                if self.find_turn_mut(turn_id).is_none() {
                    self.turns.push(Turn {
                        turn_id: turn_id.to_owned(),
                        user_text: user_text.clone(),
                        state: TimelineTurnState::Running,
                        failure: None,
                        rounds: Vec::new(),
                    });
                    self.bump();
                }
                return;
            }
            _ => {}
        }

        let Some(turn) = self.find_turn_mut(turn_id) else {
            // 快照窗口之外的迟到条目：忽略（re-baseline 会补齐权威状态）。
            return;
        };

        let mut changed = true;
        match &entry.event {
            E::TurnOpened { .. } => unreachable!(),
            E::BlockOpened { block } => {
                let round = Self::find_round_mut(turn, entry.round_num.unwrap_or(0));
                let wire = Block::from_wire(block.clone());
                match round.blocks.iter().position(|b| b.block_id == wire.block_id) {
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
            E::TextDelta { block_id, fragment_seq, delta } => {
                let round_num = entry.round_num.unwrap_or(0);
                let round = Self::find_round_mut(turn, round_num);
                if let Some(block) = Self::find_block_mut(round, block_id) {
                    // 单调 fragment 计数：重复/回放的增量被丢弃。
                    if *fragment_seq > block.last_fragment {
                        block.text.push_str(delta);
                        block.last_fragment = *fragment_seq;
                    } else {
                        changed = false;
                    }
                } else {
                    changed = false;
                }
            }
            E::BlockCheckpoint { block_id, text } => {
                let round_num = entry.round_num.unwrap_or(0);
                let round = Self::find_round_mut(turn, round_num);
                if let Some(block) = Self::find_block_mut(round, block_id) {
                    // 覆盖语义：自愈丢失/乱序的增量。
                    block.text = text.clone();
                } else {
                    changed = false;
                }
            }
            E::ToolUpdated { block_id, tool } => {
                let round_num = entry.round_num.unwrap_or(0);
                let round = Self::find_round_mut(turn, round_num);
                let card = ToolCard::from(tool.clone());
                if let Some(block) = Self::find_block_mut(round, block_id) {
                    block.tool = Some(card);
                } else {
                    changed = false;
                }
            }
            E::ToolProgress { block_id, chunk } => {
                let round_num = entry.round_num.unwrap_or(0);
                let round = Self::find_round_mut(turn, round_num);
                if let Some(block) = Self::find_block_mut(round, block_id) {
                    if let Some(tool) = block.tool.as_mut() {
                        tool.progress.push_str(chunk);
                    } else {
                        changed = false;
                    }
                } else {
                    changed = false;
                }
            }
            E::BlockSealed { block_id } => {
                let round_num = entry.round_num.unwrap_or(0);
                let round = Self::find_round_mut(turn, round_num);
                if let Some(block) = Self::find_block_mut(round, block_id) {
                    block.state = TimelineBlockState::Sealed;
                } else {
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
                changed = true;
            }
        }
        if changed {
            self.bump();
        }
    }

    /// 快照整体替换（re-baseline / 打开标签页）。
    pub fn replace_from_page(&mut self, page: &TimelinePage) {
        self.turns = page.snapshot.turns.iter().map(|t| Turn::from_wire(t.clone())).collect();
        self.has_more = page.has_more;
        self.total_turns = page.total_turns;
        self.bump();
    }

    /// 加载更早的回合（滚动上翻分页）。
    pub fn prepend_older(&mut self, page: &TimelinePage) {
        let older: Vec<Turn> = page.snapshot.turns.iter().map(|t| Turn::from_wire(t.clone())).collect();
        if older.is_empty() {
            self.has_more = false;
            self.bump();
            return;
        }
        let first_id = older[0].turn_id.clone();
        let mut merged: Vec<Turn> = older;
        for turn in self.turns.drain(..) {
            if turn.turn_id == first_id {
                // 服务端分页含边界回合：用内存中较新的版本。
                merged.pop();
                merged.push(turn);
            } else {
                merged.push(turn);
            }
        }
        self.turns = merged;
        self.has_more = page.has_more;
        self.total_turns = self.total_turns.max(page.total_turns);
        self.bump();
    }

    /// 内存中回合滑动窗口上限（对照 opencode sync 的 messages limit=100 +
    /// 窗口外裁剪）。超出时从最旧一侧丢弃并置 has_more=true——加载更早仍可用
    /// （before_turn 锚点取内存窗口首回合，服务端始终是权威历史）。
    pub fn cap_turns(&mut self, max: usize) {
        if max > 0 && self.turns.len() > max {
            let drop = self.turns.len() - max;
            self.turns.drain(..drop);
            self.has_more = true;
            self.bump();
        }
    }

    pub fn last_turn_id(&self) -> Option<&str> {
        self.turns.last().map(|t| t.turn_id.as_str())
    }

    pub fn is_streaming(&self) -> bool {
        self.turns.last().is_some_and(|t| t.is_streaming())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::timeline::{TimelineEvent, TimelineToolState};

    fn entry(seq: u64, turn: &str, event: TimelineEvent) -> TimelineEntry {
        TimelineEntry { timeline_seq: seq, turn_id: turn.to_owned(), round_num: Some(0), event }
    }

    #[test]
    fn turn_and_text_flow() {
        let mut m = TimelineModel::default();
        m.apply(&entry(1, "t1", TimelineEvent::TurnOpened { user_text: "你好".into() }));
        m.apply(&entry(2, "t1", TimelineEvent::BlockOpened {
            block: TimelineBlock {
                block_id: "b1".into(),
                block_order: 0,
                kind: TimelineBlockKind::Text,
                state: TimelineBlockState::Open,
                text: String::new(),
                tool: None,
            },
        }));
        m.apply(&entry(3, "t1", TimelineEvent::TextDelta {
            block_id: "b1".into(),
            fragment_seq: 1,
            delta: "回答".into(),
        }));
        m.apply(&entry(4, "t1", TimelineEvent::TextDelta {
            block_id: "b1".into(),
            fragment_seq: 2,
            delta: "开始".into(),
        }));
        m.apply(&entry(5, "t1", TimelineEvent::BlockCheckpoint {
            block_id: "b1".into(),
            text: "回答开始了".into(),
        }));
        m.apply(&entry(6, "t1", TimelineEvent::BlockSealed { block_id: "b1".into() }));
        m.apply(&entry(7, "t1", TimelineEvent::RoundSealed { is_final: true }));
        m.apply(&entry(8, "t1", TimelineEvent::TurnSealed {
            state: TimelineTurnState::Completed,
            failure: None,
        }));

        assert_eq!(m.turns.len(), 1);
        let turn = &m.turns[0];
        assert_eq!(turn.state, TimelineTurnState::Completed);
        assert_eq!(turn.rounds[0].blocks[0].text, "回答开始了");
        assert!(m.turns.iter().all(|t| !t.is_streaming()));
    }

    #[test]
    fn duplicate_and_replayed_entries_are_idempotent() {
        let mut m = TimelineModel::default();
        m.apply(&entry(1, "t1", TimelineEvent::TurnOpened { user_text: "hi".into() }));
        m.apply(&entry(2, "t1", TimelineEvent::BlockOpened {
            block: TimelineBlock {
                block_id: "b1".into(),
                block_order: 0,
                kind: TimelineBlockKind::Text,
                state: TimelineBlockState::Open,
                text: String::new(),
                tool: None,
            },
        }));
        let v = m.version;
        // 重复 TurnOpened → no-op
        m.apply(&entry(1, "t1", TimelineEvent::TurnOpened { user_text: "hi".into() }));
        assert_eq!(m.version, v);
        // 重复 fragment → 丢弃
        m.apply(&entry(2, "t1", TimelineEvent::TextDelta {
            block_id: "b1".into(),
            fragment_seq: 1,
            delta: "a".into(),
        }));
        m.apply(&entry(2, "t1", TimelineEvent::TextDelta {
            block_id: "b1".into(),
            fragment_seq: 1,
            delta: "a".into(),
        }));
        assert_eq!(m.turns[0].rounds[0].blocks[0].text, "a");
        // checkpoint 覆盖自愈
        m.apply(&entry(3, "t1", TimelineEvent::BlockCheckpoint {
            block_id: "b1".into(),
            text: "ab".into(),
        }));
        assert_eq!(m.turns[0].rounds[0].blocks[0].text, "ab");
    }

    #[test]
    fn tool_lifecycle_and_progress() {
        let mut m = TimelineModel::default();
        m.apply(&entry(1, "t1", TimelineEvent::TurnOpened { user_text: "run".into() }));
        m.apply(&entry(2, "t1", TimelineEvent::BlockOpened {
            block: TimelineBlock {
                block_id: "b2".into(),
                block_order: 1,
                kind: TimelineBlockKind::Tool,
                state: TimelineBlockState::Open,
                text: String::new(),
                tool: Some(TimelineTool {
                    tool_call_id: "c1".into(),
                    name: "exec".into(),
                    state: TimelineToolState::Prepared,
                    summary: None,
                    args_json: Some("{}".into()),
                    output: None,
                    diff: None,
                    progress: String::new(),
                    failure: None,
                    permission: None,
                }),
            },
        }));
        m.apply(&entry(3, "t1", TimelineEvent::ToolProgress { block_id: "b2".into(), chunk: "out1\n".into() }));
        m.apply(&entry(4, "t1", TimelineEvent::ToolProgress { block_id: "b2".into(), chunk: "out2\n".into() }));
        m.apply(&entry(5, "t1", TimelineEvent::TurnSealed {
            state: TimelineTurnState::Completed,
            failure: None,
        }));

        let tool = m.turns[0].rounds[0].blocks[0].tool.as_ref().unwrap();
        assert_eq!(tool.state, TimelineToolState::Prepared);
        assert_eq!(tool.progress, "out1\nout2\n");
    }

    #[test]
    fn cap_turns_keeps_recent_window() {
        let mut m = TimelineModel::default();
        for i in 1..=10 {
            m.apply(&entry(i, &format!("t{i}"), TimelineEvent::TurnOpened { user_text: format!("n{i}") }));
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

    #[test]
    fn prepend_older_merges_boundary() {
        let mut m = TimelineModel::default();
        m.apply(&entry(9, "t2", TimelineEvent::TurnOpened { user_text: "second".into() }));
        let page = TimelinePage {
            schema: "qaqh.Ringing".into(),
            version: 1,
            server_epoch: "ep".into(),
            seed: "0123abcd".into(),
            snapshot: crate::protocol::timeline::TimelineSnapshot {
                watermark: 8,
                turns: vec![TimelineTurn {
                    turn_id: "t1".into(),
                    created_seq: 1,
                    user_text: "first".into(),
                    sealed: true,
                    state: TimelineTurnState::Completed,
                    failure: None,
                    rounds: vec![],
                }],
            },
            has_more: false,
            total_turns: 2,
        };
        m.prepend_older(&page);
        assert_eq!(m.turns.len(), 2);
        assert_eq!(m.turns[0].user_text, "first");
        assert_eq!(m.turns[1].user_text, "second");
        assert!(!m.has_more);
    }
}
