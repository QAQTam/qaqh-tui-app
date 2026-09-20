//! UI 根布局：tabbar / 会话信息 / transcript / composer / status + 弹窗层。
//!
//! 布局函数（[`root_areas`] / [`chat_area`] / [`transcript_area`] /
//! [`transcript_content_width`]）是**唯一的布局事实源**：`draw` 与 App 的渲染
//! 缓存键都必须经它们取值。
//!
//! 它们刻意只吃基本类型（`u16` / `bool`）而非 `&App`：这样布局是纯函数、可单测，
//! 也不会因为 App 需要 `Runtime` 而无法构造。历史上 `main.rs` 用终端全宽当缓存键、
//! 而 `transcript::draw` 用扣掉侧栏后的内容宽，两者永不相等 → 缓存 100% 失效、
//! 每帧两次全量渲染。本模块的 [`transcript_content_width`] 就是为了消灭这个漂移。

pub mod activity_bar;
pub mod composer;
pub mod home;
pub mod modal;
pub mod overlays;
pub mod settings;
pub mod sidebar;
pub mod status_bar;
pub mod tab_bar;
pub mod theme;
pub mod transcript;
pub mod v2;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};

use crate::app::App;

/// 根布局切分结果。
#[derive(Debug, Clone, Copy)]
pub struct RootAreas {
    pub tab: Rect,
    pub main: Rect,
    /// ActivityBar 活动区（§4.4；F3 隐藏时高度 0，布局回收）。
    pub activity: Rect,
    pub composer: Rect,
    pub status: Rect,
}

/// 根布局切分（`draw` 与渲染缓存键共用）。
pub fn root_areas(area: Rect, composer_height: u16, activity_height: u16) -> RootAreas {
    let [tab, main, activity, composer, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(activity_height),
        Constraint::Length(composer_height),
        Constraint::Length(1),
    ])
    .areas(area);
    RootAreas {
        tab,
        main,
        activity,
        composer,
        status,
    }
}

/// chat 列（transcript 所在列；右侧 workspace 侧栏开启时扣除其宽度）。
///
/// 85 列阈值（原 100 过严，31宽侧栏在 90 列终端亦可共存；F4 显式开关优先）。
pub fn chat_area(main: Rect, show_workspace: bool) -> Rect {
    let show_ws = show_workspace && main.width >= sidebar::MIN_MAIN_WIDTH;
    if show_ws {
        let [chat, _ws] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(sidebar::PREFERRED_WIDTH),
        ])
        .areas(main);
        chat
    } else {
        main
    }
}

/// workspace 侧栏区域（chat 列右侧的剩余部分）；未展示时返回 `None`。
pub fn sidebar_area(main: Rect, show_workspace: bool) -> Option<Rect> {
    let chat = chat_area(main, show_workspace);
    (chat.width < main.width).then(|| Rect {
        x: main.x + chat.width,
        y: main.y,
        width: main.width - chat.width,
        height: main.height,
    })
}

/// chat 列内的 (会话信息行, transcript) 切分。
pub fn transcript_area(chat: Rect) -> (Rect, Rect) {
    let [info, transcript] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(chat);
    (info, transcript)
}

/// transcript 内容宽度（`transcript_viewport` 的宽度部分）。
///
/// 仅测试用：生产路径统一走 [`transcript_viewport`]（同时需要宽与高）。
#[cfg(test)]
pub fn transcript_content_width(
    area: Rect,
    composer_height: u16,
    activity_height: u16,
    show_workspace: bool,
) -> u16 {
    transcript_viewport(area, composer_height, activity_height, show_workspace).0
}

/// transcript 视口：`(内容宽, 可视行数)`。
///
/// 渲染缓存与 `transcript::draw` **必须**共用此函数：缓存要知道视口在哪，
/// 才能决定「哪些段要精确渲染、哪些只留估算高度」。两处各算一份就会漂移
/// （历史上 A1 的宽度键错位就是这么来的）。
pub fn transcript_viewport(
    area: Rect,
    composer_height: u16,
    activity_height: u16,
    show_workspace: bool,
) -> (u16, usize) {
    let root = root_areas(area, composer_height, activity_height);
    let chat = chat_area(root.main, show_workspace);
    let (_info, transcript) = transcript_area(chat);
    (
        transcript.width.saturating_sub(1),
        transcript.height as usize,
    )
}

/// 视口顶端行号（距内容顶部的行数）。
///
/// `total` 是内容总行数（对估算高度取和即可——不必先渲染）。缓存据此挑选
/// 要精确渲染的段，`transcript::draw` 据此取窗口；共用一份避免滚动位置错位。
pub fn viewport_top(total: usize, height: usize, follow: bool, offset: usize) -> usize {
    let bottom = if follow {
        0
    } else {
        offset.min(total.saturating_sub(height))
    };
    total.saturating_sub(height).saturating_sub(bottom)
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let root = root_areas(area, composer::height(app), activity_bar::height(app));

    tab_bar::draw(f, app, root.tab);

    // 首页：无 tab 时居中展示会话列表（视觉参考 opencode Home 弹性留白 + maxW）
    if app.tabs.is_empty() {
        home::draw(f, app, root.main);
    } else {
        // 三区布局：chat（左）+ workspace 侧栏（右）；composer/status 保持全宽。
        let chat = chat_area(root.main, app.show_workspace);
        let (info_area, transcript) = transcript_area(chat);

        transcript::draw_session_info(f, app, info_area);
        transcript::draw(f, app, transcript);
        if let Some(ws) = sidebar_area(root.main, app.show_workspace) {
            sidebar::draw(f, app, ws);
        }
    }

    composer::draw(f, app, root.composer);
    // §4.4 活动区：main 与 composer 之间固定 1 行（F3 显隐，隐藏时高度 0）。
    activity_bar::draw(f, app, root.activity);
    // 斜杠一级菜单浮在 composer 上方（不遮 status，不进 overlay 栈）
    composer::draw_slash_menu(f, app, root.composer);
    status_bar::draw(f, app, root.status);

    // 会话交互弹窗（active 会话的挂起交互）。
    let has_pending = app.active_session().is_some_and(|s| {
        s.active_permission().is_some() || s.pending_ask.is_some() || s.pending_plan.is_some()
    });
    if has_pending {
        modal::draw(f, app, area);
    }

    if !app.overlays.is_empty() {
        overlays::draw(f, app, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// **回归锁（A1）**：缓存键宽度必须等于 `transcript::draw` 实际使用的宽度。
    ///
    /// 直接锁住那个「任何终端尺寸都不匹配」的缺陷：只要有人改布局而忘了同步
    /// 缓存键，这里就红。
    #[test]
    fn cache_key_width_matches_transcript_draw_width() {
        for (w, h) in [
            (40u16, 20u16),
            (80, 40),
            (100, 40),
            (120, 40),
            (160, 45),
            (200, 50),
        ] {
            for show_ws in [true, false] {
                for composer_h in [3u16, 8] {
                    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
                    let mut observed: Vec<(u16, u16)> = Vec::new();
                    terminal
                        .draw(|f| {
                            let area = f.area();
                            // ① 缓存键取法（App::ensure_render_caches 用）。
                            let key = transcript_content_width(area, composer_h, 0, show_ws);
                            // ② draw 内部取法（ui::draw → transcript::draw）。
                            let root = root_areas(area, composer_h, 0);
                            let chat = chat_area(root.main, show_ws);
                            let (_i, tr) = transcript_area(chat);
                            observed.push((key, tr.width.saturating_sub(1)));
                        })
                        .unwrap();
                    for (key, draw) in observed {
                        assert_eq!(
                            key, draw,
                            "term=({w},{h}) ws={show_ws} composer={composer_h}: \
                             缓存键宽 {key} != draw 宽 {draw}（缓存将永不命中）"
                        );
                    }
                }
            }
        }
    }

    /// 侧栏区域与 chat 列互补，且不重叠（防止 draw 侧拼装 Rect 时算错）。
    #[test]
    fn sidebar_area_is_complement_of_chat() {
        for w in [60u16, 84, 85, 90, 120, 200] {
            let main = Rect::new(0, 0, w, 30);
            let chat = chat_area(main, true);
            match sidebar_area(main, true) {
                Some(ws) => {
                    assert_eq!(chat.width + ws.width, main.width);
                    assert_eq!(ws.x, chat.x + chat.width);
                }
                None => {
                    assert!(w < sidebar::MIN_MAIN_WIDTH, "w={w} 应展示侧栏");
                    assert_eq!(chat, main);
                }
            }
        }
    }

    // ───────── M2 验收：TestBackend 端到端快照 ─────────
    // 覆盖：活动区、组行、三态卡、CJK 占位几何（plan §6 M2 验收口径）。

    use crate::app::App;
    use crate::app::session::SessionState;
    use crate::app::timeline_model::{Block, Round, ToolCard, Turn};
    use qaqh_client::{
        TimelineBlockKind, TimelineBlockState, TimelineToolState, TimelineTurnState,
    };

    fn text_block(id: &str, order: u32, text: &str) -> Block {
        Block {
            block_id: id.into(),
            block_order: order,
            kind: TimelineBlockKind::Text,
            state: TimelineBlockState::Sealed,
            text: text.into(),
            tool: None,
            last_fragment: 0,
            rev: 1,
        }
    }

    fn tool_card(id: &str, name: &str, state: TimelineToolState) -> ToolCard {
        ToolCard {
            tool_call_id: id.into(),
            name: name.into(),
            state,
            summary: None,
            args_json: None,
            output: None,
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
        }
    }

    fn tool_block(id: &str, order: u32, tc: ToolCard) -> Block {
        Block {
            block_id: id.into(),
            block_order: order,
            kind: TimelineBlockKind::Tool,
            state: TimelineBlockState::Sealed,
            text: String::new(),
            tool: Some(tc),
            last_fragment: 0,
            rev: 1,
        }
    }

    fn turn(id: &str, state: TimelineTurnState, blocks: Vec<Block>) -> Turn {
        Turn {
            turn_id: id.into(),
            turn_index: None,
            user_text: "快照测试".into(),
            state,
            failure: None,
            sealed: state != TimelineTurnState::Running,
            offloaded: false,
            thinking: crate::app::timeline_model::ThinkingStats::default(),
            rounds: vec![Round {
                round_num: 0,
                sealed: true,
                is_final: true,
                blocks,
            }],
        }
    }

    fn app_with(turns: Vec<Turn>) -> App {
        let (mut app, _rx) = App::new_for_test();
        app.tabs.push("seed".into());
        let mut sess = SessionState::new("seed".into());
        sess.timeline.turns = turns;
        app.sessions.insert("seed".into(), sess);
        app.active = 0;
        app
    }

    /// 渲染一帧并提取纯文本（每行 trim 尾；宽字符后继占位 cell 不重复拼入）。
    fn draw_text(app: &mut App, w: u16, h: u16) -> String {
        let area = Rect::new(0, 0, w, h);
        app.ensure_render_caches(area);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            let mut line = String::new();
            let mut skip_next = false;
            for x in 0..buf.area.width {
                let sym = buf[(x, y)].symbol();
                if skip_next {
                    skip_next = false;
                    if sym == " " || sym.is_empty() {
                        continue;
                    }
                }
                line.push_str(sym);
                use unicode_width::UnicodeWidthChar;
                if sym.chars().next().is_some_and(|c| c.width() == Some(2)) {
                    skip_next = true;
                }
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    /// 活动区：运行中工具的 icon+name+摘要（无滚动、无卡内重复——卡标题是
    /// `· name` / `# name`，活动行是 `$ name summary`）。
    #[test]
    fn snapshot_activity_bar_shows_running_tool() {
        let mut tc = tool_card("t1", "bash", TimelineToolState::Running);
        tc.summary = Some("cargo build".into());
        let mut app = app_with(vec![turn(
            "turn-1",
            TimelineTurnState::Running,
            vec![tool_block("b1", 0, tc)],
        )]);
        let text = draw_text(&mut app, 100, 30);
        assert!(
            text.lines().any(|l| l.contains("$ bash cargo build")),
            "活动行应显示运行中工具：\n{text}"
        );

        // F3 关闭：活动行不占位，`$ bash` 前缀消失。
        app.show_activity = false;
        let text2 = draw_text(&mut app, 100, 30);
        assert!(
            !text2.lines().any(|l| l.contains("$ bash")),
            "F3 关闭后活动行应消失：\n{text2}"
        );
    }

    /// 组行：连续工具折叠为一行 `┃ ⚙ N tool calls · …`。
    #[test]
    fn snapshot_group_line_folded() {
        let mut app = app_with(vec![turn(
            "turn-1",
            TimelineTurnState::Completed,
            vec![
                tool_block(
                    "g1",
                    0,
                    tool_card("g1", "read", TimelineToolState::Succeeded),
                ),
                tool_block(
                    "g2",
                    1,
                    tool_card("g2", "grep", TimelineToolState::Succeeded),
                ),
                tool_block("g3", 2, tool_card("g3", "edit", TimelineToolState::Failed)),
            ],
        )]);
        let text = draw_text(&mut app, 100, 30);
        let group = text
            .lines()
            .find(|l| l.contains("tool calls"))
            .unwrap_or_else(|| panic!("应有组行：\n{text}"));
        assert!(
            group.contains("3 tool calls") && group.contains("2✓") && group.contains("1✗"),
            "组行计数：{group}"
        );
    }

    /// 三态卡：展开组后 Running / Succeeded / Failed 卡各自的状态行可见
    /// （Block 形态——长输出；inline 形态用 icon 表状态，无文字）。
    #[test]
    fn snapshot_three_state_cards() {
        let long = || "line1\nline2\nline3\nline4\nline5".to_string();
        let mut c1 = tool_card("c1", "bash", TimelineToolState::Running);
        c1.progress = long();
        let mut c2 = tool_card("c2", "read", TimelineToolState::Succeeded);
        c2.output = Some(long());
        let mut c3 = tool_card("c3", "edit", TimelineToolState::Failed);
        c3.output = Some(long());
        let mut app = app_with(vec![turn(
            "turn-1",
            TimelineTurnState::Completed,
            vec![
                tool_block("c1", 0, c1),
                tool_block("c2", 1, c2),
                tool_block("c3", 2, c3),
            ],
        )]);
        app.sessions
            .get_mut("seed")
            .expect("session")
            .expanded_groups
            .insert(("turn-1".to_string(), 0));
        let text = draw_text(&mut app, 100, 50);
        for needle in [" running", " completed", " failed"] {
            assert!(
                text.lines().any(|l| l.contains(needle)),
                "三态卡缺少 {needle:?}：\n{text}"
            );
        }
    }

    /// CJK 占位几何：宽字符正文渲染 + 运行中卡的动画槽位共存不破位。
    #[test]
    fn snapshot_cjk_geometry() {
        let mut tc = tool_card("t1", "bash", TimelineToolState::Running);
        tc.progress = "编译中…".into();
        let mut app = app_with(vec![turn(
            "turn-1",
            TimelineTurnState::Running,
            vec![
                text_block("b0", 0, "中文内容测试行"),
                tool_block("b1", 1, tc),
            ],
        )]);
        let text = draw_text(&mut app, 80, 30);
        assert!(text.contains("中文内容测试行"), "CJK 正文应可见：\n{text}");

        // 宽字符占位：CJK 字符所在 cell 的后继 cell 为空串（宽字符占 2 列）。
        let area = Rect::new(0, 0, 80, 30);
        app.ensure_render_caches(area);
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let mut found = false;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width.saturating_sub(1) {
                let sym = buf[(x, y)].symbol();
                if sym.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)) {
                    let next = buf[(x + 1, y)].symbol();
                    assert_eq!(
                        next, " ",
                        "CJK 宽字符后继 cell 应为空白占位（EMPTY）：({x},{y})={sym:?} next={next:?}"
                    );
                    found = true;
                    break;
                }
            }
            if found {
                break;
            }
        }
        assert!(found, "快照中应有 CJK 字符");
    }
}
