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
use std::time::{Duration, Instant};

use super::*;
use crate::app::anim;
use crate::app::session::SessionState;
use crate::app::timeline_model::{Block, Round, ToolCard};
use crate::theme::Theme;
use crate::ui::v2::runtime::V2TranscriptRuntime;
use crate::ui::v2::transcript::render_transcript;
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

// ── 基准 4：V2 projector + commit ledger + render ─────────────────

#[test]
#[ignore = "基准：cargo test render::bench -- --ignored --nocapture"]
fn bench_v2_commit_runtime() {
    let _guard = BENCH_LOCK.lock().unwrap();
    const TURNS: usize = 110;
    const TOOLS: usize = 20;

    let mut sess = gen_session(TURNS, TOOLS);
    let turns = &sess.timeline.turns;

    let mut v1_cache = TranscriptCache::new(WIDTH);
    let v1_start = Instant::now();
    let _ = refresh(&sess, WIDTH, None, &mut v1_cache);
    let v1_first_frame = v1_start.elapsed();

    let lazy_start = Instant::now();
    let mut lazy_runtime = V2TranscriptRuntime::new();
    lazy_runtime.begin_replay("bench");
    let lazy_pending = lazy_runtime.replay_slice("bench", &turns[..8.min(turns.len())]);
    let lazy_blocks: Vec<_> = lazy_pending
        .iter()
        .take(COMMIT_CHUNK_BLOCKS_FOR_BENCH)
        .map(|pending| pending.block.clone())
        .collect();
    let _ = render_transcript(&lazy_blocks, WIDTH, Theme::current());
    let lazy_first_frame = lazy_start.elapsed();

    reset_counters();
    let replay_start = Instant::now();
    let mut runtime = V2TranscriptRuntime::new();
    let emitted = runtime.replay_from_scratch_versioned("bench", turns, sess.timeline.version);
    let replay_elapsed = replay_start.elapsed();
    let runtime_live = live();
    let runtime_peak = peak();

    let incremental_start = Instant::now();
    let incremental = runtime.sync_timeline_versioned("bench", turns, sess.timeline.version);
    let incremental_elapsed = incremental_start.elapsed();
    assert!(incremental.is_empty(), "高水位重放后不得重复 emit");

    const DELTAS: usize = 20;
    let delta_start = Instant::now();
    for _ in 0..DELTAS {
        push_delta(&mut sess);
        sess.timeline.version = sess.timeline.version.saturating_add(1);
        let _ =
            runtime.sync_timeline_versioned("bench", &sess.timeline.turns, sess.timeline.version);
    }
    let delta_elapsed = delta_start.elapsed();

    let blocks: Vec<_> = emitted
        .iter()
        .map(|pending| pending.block.clone())
        .collect();
    let first_chunk_start = Instant::now();
    let first_chunk: Vec<_> = emitted
        .iter()
        .take(COMMIT_CHUNK_BLOCKS_FOR_BENCH)
        .map(|pending| pending.block.clone())
        .collect();
    let _ = render_transcript(&first_chunk, WIDTH, Theme::current());
    let first_chunk_elapsed = first_chunk_start.elapsed();
    let render_start = Instant::now();
    let lines = render_transcript(&blocks, WIDTH, Theme::current());
    let render_elapsed = render_start.elapsed();
    let v2_first_frame = replay_elapsed + first_chunk_elapsed;

    // ── 渲染拆分（M6.4）：全量读数把「每帧热路径」与「历史提交」混在一起 ──
    //
    // 生产有两条**完全不同**的渲染路径（`terminal/agent.rs`）：
    //   ① 每帧 live viewport（:784）：只渲染 `BlockState::Live` 的块，即
    //      **正在进行的那个回合**，然后按视口行数切片；
    //   ② scrollback commit（:584）：每次最多 `COMMIT_CHUNK_BLOCKS` 块，
    //      分块把历史写进 scrollback。
    //
    // 旧的 `render: 24432 lines / 841 ms` 是**整段历史**的一次性总成本，
    // 既不是每帧成本、也不是单块成本 —— 拿它当"渲染开销"会严重高估热路径。
    // 这里把两者分开量。
    // ① 每帧 live viewport：用**最后一个回合的块**近似其载荷 —— 生产里 Live
    //    块就是当前回合尚未封口的部分（每回合 ≈ TOOLS 工具 + 文本/思考）。
    let live_turn_blocks: Vec<_> = blocks.iter().rev().take(TOOLS + 2).rev().cloned().collect();
    let live_render_start = Instant::now();
    let live_lines = render_transcript(&live_turn_blocks, WIDTH, Theme::current());
    let live_render_elapsed = live_render_start.elapsed();

    let chunk_start = Instant::now();
    let _ = render_transcript(&first_chunk, WIDTH, Theme::current());
    let chunk_elapsed = chunk_start.elapsed();

    let per_chunk = render_elapsed.as_secs_f64() * 1000.0
        / (blocks.len() as f64 / COMMIT_CHUNK_BLOCKS_FOR_BENCH as f64).max(1.0);

    println!(
        "\n=== V2 commit runtime（{TURNS} turns × {TOOLS} tools）===\n\
         ⑤ replay: {} blocks / {:.2} ms；runtime live {} / peak {}\n\
           高水位增量: {} blocks / {} µs\n\
           live delta ×{DELTAS}: avg {} µs/帧\n\
           render 拆分: live viewport {} 块 → {} 行 / {:.2} ms（每帧热路径）\n\
                        commit chunk {} 块 / {:.2} ms（每次提交；均值 {:.2} ms）\n\
                        全量 {} 块 → {} 行 / {:.2} ms（分块摊还）\n\
           first frame: v1 cache {:.2} ms | v2 lazy replay+chunk {:.2} ms | \
           v2 full replay+chunk {:.2} ms",
        emitted.len(),
        replay_elapsed.as_secs_f64() * 1000.0,
        kb(runtime_live),
        kb(runtime_peak),
        incremental.len(),
        incremental_elapsed.as_micros(),
        delta_elapsed.as_micros() / DELTAS as u128,
        live_turn_blocks.len(),
        live_lines.len(),
        live_render_elapsed.as_secs_f64() * 1000.0,
        first_chunk.len(),
        chunk_elapsed.as_secs_f64() * 1000.0,
        per_chunk,
        blocks.len(),
        lines.len(),
        render_elapsed.as_secs_f64() * 1000.0,
        v1_first_frame.as_secs_f64() * 1000.0,
        lazy_first_frame.as_secs_f64() * 1000.0,
        v2_first_frame.as_secs_f64() * 1000.0,
    );
}

const COMMIT_CHUNK_BLOCKS_FOR_BENCH: usize = 32;

#[test]
#[ignore = "基准：cargo test render::bench -- --ignored --nocapture"]
fn bench_v2_runtime_scale_curve() {
    let _guard = BENCH_LOCK.lock().unwrap();
    const TOOLS: usize = 20;
    const DELTAS: usize = 20;

    println!("\n=== V2 commit runtime 规模曲线（每回合 {TOOLS} 工具）===");
    reset_counters();
    println!(
        "{:>6} {:>8} {:>12} {:>13} {:>15} {:>12}",
        "turns", "blocks", "replay ms", "live delta µs", "render ms", "steady live"
    );

    for &turns_n in &[25usize, 50, 110, 220, 440] {
        let mut sess = gen_session(turns_n, TOOLS);

        let live_before = live();
        let replay_start = Instant::now();
        let mut runtime = V2TranscriptRuntime::new();
        let emitted = runtime.replay_from_scratch_versioned(
            "bench",
            &sess.timeline.turns,
            sess.timeline.version,
        );
        let emitted_len = emitted.len();
        let replay_elapsed = replay_start.elapsed();

        let delta_start = Instant::now();
        for _ in 0..DELTAS {
            push_delta(&mut sess);
            sess.timeline.version = sess.timeline.version.saturating_add(1);
            let _ = runtime.sync_timeline_versioned(
                "bench",
                &sess.timeline.turns,
                sess.timeline.version,
            );
        }
        let delta_elapsed = delta_start.elapsed();

        let blocks: Vec<_> = emitted
            .iter()
            .map(|pending| pending.block.clone())
            .collect();
        let render_start = Instant::now();
        let lines = render_transcript(&blocks, WIDTH, Theme::current());
        let render_elapsed = render_start.elapsed();
        drop(lines);
        drop(blocks);
        drop(emitted);
        let runtime_steady = live().saturating_sub(live_before);

        println!(
            "{turns_n:>6} {:>8} {:>12.2} {:>13} {:>15.2} {:>12}",
            emitted_len,
            replay_elapsed.as_secs_f64() * 1000.0,
            delta_elapsed.as_micros() / DELTAS as u128,
            render_elapsed.as_secs_f64() * 1000.0,
            kb(runtime_steady),
        );
    }
}

// ── M6.4 性能门禁 ────────────────────────────────────────────────────
//
// **为什么单独一个测试、且要单线程跑**：内存读数走全局分配器计数，`cargo test`
// 默认并行会让其它测试的分配污染 `live()`。所以本门禁由
// `scripts/perf-gate.sh` 用 `--test-threads=1 --ignored` 拉起，并已接进
// `scripts/ci-linux.sh`。
//
// **只钉结构性性质（数量级），不钉绝对耗时**：powersave/boost 与 CI 机器差异
// 能让同一场景差几倍（见 M6.4 报告 §0）。阈值同时满足两条——离实测有 1~2 个
// 数量级余量，离**被修复过的回归形态**也有 1 个数量级，所以真退化一定抓得到：
//
// | 判据 | 实测（2026-09-21） | 阈值 | 回归形态 |
// |---|---|---|---|
// | version 未变同步 | 3 µs | < 1 ms | 整历史重扫 27.7 ms/帧 |
// | live delta/帧 | 7–28 µs | < 2 ms | 每帧随历史线性增长 |
// | 440 回合常驻 | 545 KB | < 8 MB | 每 block 存完整 seed/turn/block 字符串 |
// | 440/110 块数比 | 3.996 | 3.5–4.5 | 有界队列把长 replay 截断（4096 上限） |
// | 440 回合 replay | 104 ms | < 3 s | 超线性扫描 |
#[test]
#[ignore = "性能门禁：scripts/perf-gate.sh（--test-threads=1 --ignored）"]
fn perf_gate_v2_runtime() {
    let _guard = BENCH_LOCK.lock().unwrap();
    const TOOLS: usize = 20;
    const DELTAS: usize = 20;

    // ── ① version 未变时的同步必须 O(1) ──
    let mut sess = gen_session(110, TOOLS);
    let version = sess.timeline.version;
    let mut runtime = V2TranscriptRuntime::new();
    let first = runtime.replay_from_scratch_versioned("bench", &sess.timeline.turns, version);
    assert!(!first.is_empty(), "replay 必须产出待提交块");
    let noop_start = Instant::now();
    let noop = runtime.sync_timeline_versioned("bench", &sess.timeline.turns, version);
    let noop_elapsed = noop_start.elapsed();
    assert!(noop.is_empty(), "version 未变时不得重复 emit");
    assert!(
        noop_elapsed < Duration::from_millis(1),
        "version 未变时的同步必须 O(1)：实测 {noop_elapsed:?}（阈值 1ms；\
         回归形态是整历史重扫 ~27.7ms/帧）"
    );

    // ── ② live delta 必须微秒级 ──
    let delta_start = Instant::now();
    for _ in 0..DELTAS {
        push_delta(&mut sess);
        sess.timeline.version = sess.timeline.version.saturating_add(1);
        let _ =
            runtime.sync_timeline_versioned("bench", &sess.timeline.turns, sess.timeline.version);
    }
    let per_frame = delta_start.elapsed() / DELTAS as u32;
    assert!(
        per_frame < Duration::from_millis(2),
        "live delta 必须微秒级：实测 {per_frame:?}/帧（阈值 2ms）"
    );

    // ── ③ 规模线性：块数必须随回合数成比例 ──
    let small = gen_session(110, TOOLS);
    let mut small_rt = V2TranscriptRuntime::new();
    let small_blocks = small_rt
        .replay_from_scratch_versioned("bench", &small.timeline.turns, small.timeline.version)
        .len();

    // ── ④ 长会话常驻内存 + replay 时间上界 ──
    // `live_before` 扣掉会话模型本身的分配，只留 runtime 常驻（与 §2 曲线同口径）。
    let big = gen_session(440, TOOLS);
    let live_before = live();
    let big_start = Instant::now();
    let mut big_rt = V2TranscriptRuntime::new();
    let big_blocks = big_rt
        .replay_from_scratch_versioned("bench", &big.timeline.turns, big.timeline.version)
        .len();
    let big_elapsed = big_start.elapsed();
    let steady = live().saturating_sub(live_before);

    assert!(
        steady < 8 * 1024 * 1024,
        "440 回合 runtime 常驻必须 < 8MB：实测 {}（{big_blocks} blocks；\
         回归形态是每 block 存完整身份字符串）",
        kb(steady)
    );
    assert!(
        big_elapsed < Duration::from_secs(3),
        "440 回合 replay 必须 < 3s：实测 {big_elapsed:?}（基线 ~104ms）"
    );

    let ratio = big_blocks as f64 / small_blocks as f64;
    assert!(
        (3.5..=4.5).contains(&ratio),
        "块数必须随回合数线性（440/110 应≈4）：实测 {ratio:.3}\
         （比值塌陷 = 有界队列把长 replay 截断了）"
    );

    println!(
        "\n=== M6.4 性能门禁（{TOOLS} tools/回合）===\n\
         version 未变同步: {noop_elapsed:?}（阈值 <1ms）\n\
         live delta/帧:   {per_frame:?}（阈值 <2ms）\n\
         440 回合常驻:    {} / {big_blocks} blocks（阈值 <8MB）\n\
         440 回合 replay: {big_elapsed:?}（阈值 <3s）\n\
         块数线性度:      {ratio:.3}（阈值 3.5–4.5）",
        kb(steady)
    );
}
