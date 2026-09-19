//! M1 基准（plan §6 附录）：长上下文内存曲线 + 流式/多工具刷新成本。
//!
//! 运行（ignored，不影响门禁）：
//! ```text
//! cargo test render::bench -- --ignored --nocapture
//! ```
//!
//! 测量手段（std-only，无新依赖）：
//! - **内存**：`#[global_allocator]` 计数分配器（LIVE=当前存活字节，PEAK=峰值）。
//!   曲线读的是**净增存活**（缓存保留的分配）与**阶段峰值**（含临时分配）；
//! - **时间**：`Instant`，流式模拟逐 delta 计时。
//!
//! 场景参数对照真实会话 `5e34cca4`（110 回合 / 2500+ 工具调用 / 思考为主）：
//! 每回合 = 思考块(~3000 CJK 字符) + 20 个工具块(12 行输出) + 回答(~800 CJK 字符)。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use super::*;
use crate::app::anim;
use crate::app::session::SessionState;
use crate::app::timeline_model::{Block, Round, ToolCard};
use qaqh_client::{TimelineBlockKind, TimelineBlockState, TimelineToolState, TimelineTurnState};

// ── 计数分配器 ──────────────────────────────────────────────────────

pub struct CountingAlloc;

pub static LIVE: AtomicUsize = AtomicUsize::new(0);
pub static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let cur = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(cur, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) };
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

/// 两个基准互斥（分配器计数是全局的，并行会互相污染）。
static BENCH_LOCK: Mutex<()> = Mutex::new(());

fn kb(n: usize) -> String {
    format!("{:.0} KB", n as f64 / 1024.0)
}

fn reset_counters() {
    LIVE.store(0, Ordering::Relaxed);
    PEAK.store(0, Ordering::Relaxed);
}

fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}

// ── 长会话生成器（对照真实会话轮廓） ────────────────────────────────

fn reasoning_block(id: &str, order: u32, chars: usize) -> Block {
    let text = "梳理当前局面并核对约束条件。".repeat(chars.div_ceil(14));
    Block {
        block_id: id.to_string(),
        block_order: order,
        kind: TimelineBlockKind::Reasoning,
        state: TimelineBlockState::Sealed,
        text,
        tool: None,
        last_fragment: 0,
        rev: 1,
    }
}

fn text_block(id: &str, order: u32, state: TimelineBlockState, chars: usize) -> Block {
    Block {
        block_id: id.to_string(),
        block_order: order,
        kind: TimelineBlockKind::Text,
        state,
        text: format!("回答正文{}", "内容段落。".repeat(chars.div_ceil(5))),
        tool: None,
        last_fragment: 0,
        rev: 1,
    }
}

fn tool_block(id: &str, order: u32, output_lines: usize, state: TimelineToolState) -> Block {
    let output = (0..output_lines)
        .map(|i| format!("line {i}: -rw-r--r-- 1 user group 4096 Sep 18 file_{i}.rs"))
        .collect::<Vec<_>>()
        .join("\n");
    Block {
        block_id: id.to_string(),
        block_order: order,
        kind: TimelineBlockKind::Tool,
        state: TimelineBlockState::Sealed,
        text: String::new(),
        tool: Some(ToolCard {
            tool_call_id: id.to_string(),
            name: if order.is_multiple_of(2) {
                "bash"
            } else {
                "read"
            }
            .to_string(),
            state,
            summary: Some("src/app/render/mod.rs".to_string()),
            args_json: None,
            output: Some(output),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
        }),
        last_fragment: 0,
        rev: 1,
    }
}

/// 每回合：思考(≈3K CJK) + `tools` 个工具块(12 行输出) + 回答(≈800 CJK)。
fn gen_session(turns: usize, tools: usize) -> SessionState {
    let mut s = SessionState::new("bench-seed".to_string());
    for i in 0..turns {
        let mut blocks = vec![reasoning_block(&format!("r{i}"), 0, 3000)];
        for t in 0..tools {
            blocks.push(tool_block(
                &format!("c{i}_{t}"),
                t as u32 + 1,
                12,
                TimelineToolState::Succeeded,
            ));
        }
        blocks.push(text_block(
            &format!("a{i}"),
            tools as u32 + 1,
            TimelineBlockState::Sealed,
            800,
        ));
        s.timeline.turns.push(Turn {
            thinking: Default::default(),
            turn_id: format!("t{i}"),
            turn_index: Some(i as u64),
            user_text: format!("帮我处理任务 {i}，注意边界条件"),
            state: TimelineTurnState::Completed,
            failure: None,
            sealed: true,
            offloaded: false,
            rounds: vec![Round {
                round_num: 0,
                sealed: true,
                is_final: false,
                blocks,
            }],
        });
    }
    // 活动回合：流式中（open 文本 + running 工具）。
    s.timeline.turns.push(Turn {
        thinking: Default::default(),
        turn_id: "t_active".to_string(),
        turn_index: None,
        user_text: "继续".to_string(),
        state: TimelineTurnState::Running,
        failure: None,
        sealed: false,
        offloaded: false,
        rounds: vec![Round {
            round_num: 0,
            sealed: false,
            is_final: false,
            blocks: vec![
                reasoning_block("ra", 0, 3000),
                tool_block("ca_run", 1, 12, TimelineToolState::Running),
                text_block("ta", 2, TimelineBlockState::Open, 200),
            ],
        }],
    });
    s
}

/// 向活动回合追加 `n` 个已封口工具块（多工具调用压力，插到流式块之前）。
fn add_multitool(sess: &mut SessionState, n: usize) {
    let turn = sess.timeline.turns.last_mut().unwrap();
    let round = turn.rounds.last_mut().unwrap();
    let mut extra: Vec<Block> = (0..n)
        .map(|t| {
            tool_block(
                &format!("cx{t}"),
                t as u32,
                12,
                TimelineToolState::Succeeded,
            )
        })
        .collect();
    extra.extend(std::mem::take(&mut round.blocks));
    round.blocks = extra;
}

const WIDTH: u16 = 100;
const VIEW_H: usize = 40;

fn bottom_viewport(total: usize) -> Option<(usize, usize)> {
    Some((total.saturating_sub(VIEW_H), VIEW_H))
}

// ── 基准 1：长上下文内存曲线 ────────────────────────────────────────

#[test]
#[ignore = "基准：cargo test render::bench -- --ignored --nocapture"]
fn bench_long_context_memory_curve() {
    let _guard = BENCH_LOCK.lock().unwrap();
    println!("\n=== 长上下文内存曲线（width={WIDTH}，视口=底部 {VIEW_H} 行，每回合 20 工具）===");
    println!(
        "{:>6} {:>10} {:>16} {:>12} {:>14} {:>10}",
        "turns", "model", "新·全驻留", "新·淘汰", "新·淘汰峰值", "驻留/总块"
    );

    for &n in &[25usize, 50, 110, 220, 440] {
        reset_counters();
        let sess = gen_session(n, 20);
        let model = live();

        // 全驻留口径（历史对照值见 plan 附录 A：旧回合粒度缓存同期读数）。
        let base_new = live();
        let mut cache_full = TranscriptCache::new(WIDTH);
        let _ = refresh(&sess, WIDTH, None, &mut cache_full);
        let new_full = live() - base_new;

        // 视口淘汰口径（从零直接建——不等价于旧路径的首帧行为）。
        let base_new2 = live();
        let mut cache = TranscriptCache::new(WIDTH);
        let stats = refresh(
            &sess,
            WIDTH,
            bottom_viewport(cache.total_lines()),
            &mut cache,
        );
        let new_evict = live() - base_new2;
        let new_peak = peak().saturating_sub(base_new2);
        let total_blocks = (n + 1) * 24;

        println!(
            "{:>6} {:>10} {:>16} {:>12} {:>14} {:>10}",
            n,
            kb(model),
            kb(new_full),
            kb(new_evict),
            kb(new_peak),
            format!("{}/{}", stats.resident_blocks, total_blocks),
        );
    }
}

// ── 基准 2：流式 + 多工具刷新成本 ──────────────────────────────────

fn push_delta(sess: &mut SessionState) {
    let b = sess
        .timeline
        .turns
        .last_mut()
        .unwrap()
        .rounds
        .last_mut()
        .unwrap()
        .blocks
        .last_mut()
        .unwrap();
    b.text.push('字');
    b.rev += 1;
}

#[test]
#[ignore = "基准：cargo test render::bench -- --ignored --nocapture"]
fn bench_streaming_and_multitool() {
    let _guard = BENCH_LOCK.lock().unwrap();
    const HISTORY: usize = 110;
    const DELTAS: usize = 20;

    println!("\n=== 流式刷新成本（历史 {HISTORY} 回合 × 20 工具，活动回合 50 工具 + 流式文本）===");
    let mut sess = gen_session(HISTORY, 20);
    add_multitool(&mut sess, 50);

    let mut cache = TranscriptCache::new(WIDTH);
    // 预热：先建全量几何（总行数），再按底部视口淘汰离屏。
    refresh(&sess, WIDTH, None, &mut cache);
    let total = cache.total_lines();
    refresh(&sess, WIDTH, bottom_viewport(total), &mut cache);

    // ① 流式：逐 delta（文本追加 + rev）。
    let (mut new_us, mut new_rebuilt) = (0u128, 0usize);
    for _ in 0..DELTAS {
        push_delta(&mut sess);
        let t0 = Instant::now();
        let st = refresh(&sess, WIDTH, bottom_viewport(total), &mut cache);
        new_us += t0.elapsed().as_micros();
        new_rebuilt += st.rebuilt_blocks;
    }
    println!(
        "① delta 流式 ×{DELTAS}: avg {}µs/帧（重渲块 {new_rebuilt}，每 delta 恰 1）",
        new_us / DELTAS as u128,
    );

    // ② Tick：只有帧号变化（spinner 转动），内容零变化。
    let (mut new_us, mut new_rebuilt) = (0u128, 0usize);
    for f in 0..40u64 {
        anim::frame_override::set(f);
        let t0 = Instant::now();
        new_rebuilt += refresh(&sess, WIDTH, bottom_viewport(total), &mut cache).rebuilt_blocks;
        new_us += t0.elapsed().as_micros();
    }
    anim::frame_override::clear();
    println!(
        "② tick 帧动 ×40: avg {}µs/帧（重渲块 {new_rebuilt}，应为 0）",
        new_us / 40,
    );

    // ③ 多工具回合冷重渲：活动回合所有块从估算恢复到精确。
    let mut cold_cache = TranscriptCache::new(WIDTH);
    refresh(&sess, WIDTH, bottom_viewport(total), &mut cold_cache);
    let t0 = Instant::now();
    let st_new = refresh(&sess, WIDTH, bottom_viewport(total), &mut cold_cache);
    let new_full = t0.elapsed().as_micros();
    println!(
        "③ 热路径重入（50 工具活动回合）: {new_full}µs（重渲块 {}）",
        st_new.rebuilt_blocks,
    );
}

// ── 基准 3：长流式文本的尾部增量（T7）──────────────────────────────

#[test]
#[ignore = "基准：cargo test render::bench -- --ignored --nocapture"]
fn bench_long_stream_delta() {
    let _guard = BENCH_LOCK.lock().unwrap();
    const INIT: usize = 8192; // ~8KB 正文（约 100+ 折行行）
    const DELTAS: usize = 20;

    // 增量（T7 形态）：init 一次，逐 delta carry —— 只重折尾行。
    let mut b_inc = text_block("long", 0, TimelineBlockState::Open, INIT);
    let mut seg_inc = BlockSeg {
        key: 0,
        body: BlockBody::Height(0),
        anim: Vec::new(),
        stream: None,
    };
    stream::init(&mut seg_inc, &b_inc, WIDTH);
    let mut inc_us = 0u128;
    let mut inc_sealed = 0usize;
    for _ in 0..DELTAS {
        b_inc.text.push('字');
        b_inc.rev += 1;
        let t0 = Instant::now();
        let ok = stream::carry(&mut seg_inc, &b_inc, WIDTH);
        inc_us += t0.elapsed().as_micros();
        assert!(ok, "增量必须可续算");
    }
    if let BlockBody::Streaming { prefix, .. } = &seg_inc.body {
        inc_sealed = prefix.len();
    }

    // 全量对照（pre-T7 形态等价）：每个 delta 从全文重折 + 重建行体。
    let mut b_full = text_block("long", 0, TimelineBlockState::Open, INIT);
    let mut seg_full = BlockSeg {
        key: 0,
        body: BlockBody::Height(0),
        anim: Vec::new(),
        stream: None,
    };
    let mut full_us = 0u128;
    for _ in 0..DELTAS {
        b_full.text.push('字');
        b_full.rev += 1;
        let t0 = Instant::now();
        stream::init(&mut seg_full, &b_full, WIDTH);
        full_us += t0.elapsed().as_micros();
    }

    println!(
        "\n=== 长流式文本尾部增量（初始 {INIT} 字符 × delta {DELTAS}）===\n\
          ④ 增量(carry) avg {}µs/delta（封口 {inc_sealed} 行） | 全量(init) avg {}µs/delta",
        inc_us / DELTAS as u128,
        full_us / DELTAS as u128,
    );
}
