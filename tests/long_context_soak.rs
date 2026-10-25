//! 长上下文压力验证(reducer 层,headless)。
//!
//! 回答的问题:**喂完整场大会话,内存与 CPU 是否有界、不崩?**
//!
//! 方法与边界(先说清楚测不了什么):
//! - 本测试独占一个测试二进制(文件里只有一个 `#[test]`),全局分配器的
//!   计数不被并发测试污染 —— 这是把内存数字当真话的前提。
//! - 覆盖的是 **reducer 数据面**(`apply_entry`/`apply_page` 正是 SSE 事件
//!   落地的同一条路径):CPU 总耗时 + 峰值分配 + 保留字节三条硬断言。
//! - **测不了渲染帧率**:lazy 的缓存依赖 iced runtime 的 widget Tree 跨帧
//!   存活,脱离 runtime 调 `view()` 每次都是空缓存,测出来的是未加速路径。
//!   帧率只能真机体感或将来上 iced_tester。
//!
//! 断言阈值不是拍脑袋,是有算术的(见测试内注释):
//! 有界设计 ≈ 12–20 MB,无界设计 ≈ 55–70 MB —— 阈值卡在中间,两个方向
//! 都留了余量,防「阈值定得太松/太紧」自欺。
//! 实测基线(5k 回合 / 13 万事件):~3s、peak ~40 MB、retained ~8 MB
//! (debug 构建;其中 peak 大头是 markdown 逐项结构与工具卡克隆的瞬态)。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use qaqh_client::{
    TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
    TimelineTool, TimelineToolState, TimelineTurnState,
};
use qaqh_iced::session::{Block, SessionState, Turn};

// ── 计数分配器:当前存活字节 + 峰值 ──

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: 委托 System 分配器,前提与 System 相同。
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            // 饱和算术:计数即便失衡也只表现为偏大,绝不回绕成天文数字,
            // 用溢出 panic 把「谁失衡」的真问题淹没。
            let now = CURRENT
                .fetch_add(layout.size(), Ordering::Relaxed)
                .saturating_add(layout.size());
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // 饱和减法:失衡时打点(而不是让计数回绕、在别处炸出假症状)。
        let previous = CURRENT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_sub(layout.size()))
            })
            .unwrap_or_default();
        if previous < layout.size() {
            eprintln!("[counting-alloc] dealloc 下溢:size={}", layout.size());
        }
        // SAFETY: 指针来自 System.alloc,layout 与之一致。
        unsafe { System.dealloc(pointer, layout) }
    }

    // 刻意**不**覆写 realloc:默认实现走 alloc + copy + dealloc,
    // 经由上面两个钩子计数,天然平衡 —— 手写「先减旧再加新」已在
    // 第一版实测失衡(下溢回绕 → alloc 溢出 panic),不值得再来一次。
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// ── 场景规模 ──

const TURNS: usize = 5_000;
const DELTAS_PER_TURN: usize = 20;
const TOOL_EVERY: usize = 10; // → 500 个工具调用
const TOOL_OUTPUT_CHARS: usize = 100_000; // 单工具未截断时 100 KB

// 变量分离开关(归因用):SOAK_DELTAS=0 / SOAK_TOOLS=1000000 可单独关掉
// markdown 流或工具流,看每回合增长归谁。
fn scenario() -> (usize, usize) {
    let deltas = std::env::var("SOAK_DELTAS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DELTAS_PER_TURN);
    let tool_every = std::env::var("SOAK_TOOLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(TOOL_EVERY);
    (deltas, tool_every)
}

// ── 事件构造 ──

fn entry(turn: usize, event: TimelineEvent) -> TimelineEntry {
    TimelineEntry {
        timeline_seq: 1,
        turn_id: format!("turn-{turn}"),
        round_num: Some(0),
        event,
    }
}

fn text_block_open() -> TimelineEvent {
    TimelineEvent::BlockOpened {
        block: TimelineBlock {
            block_id: "text".into(),
            block_order: 0,
            kind: TimelineBlockKind::Text,
            state: TimelineBlockState::Open,
            text: String::new(),
            tool: None,
            reasoning_kind: None,
        },
    }
}

fn tool_block_open() -> TimelineEvent {
    TimelineEvent::BlockOpened {
        block: TimelineBlock {
            block_id: "tool".into(),
            block_order: 1,
            kind: TimelineBlockKind::Tool,
            state: TimelineBlockState::Open,
            text: String::new(),
            tool: None,
            reasoning_kind: None,
        },
    }
}

fn tool_updated() -> TimelineEvent {
    TimelineEvent::ToolUpdated {
        block_id: "tool".into(),
        tool: TimelineTool {
            tool_call_id: "call".into(),
            name: "exec".into(),
            state: TimelineToolState::Succeeded,
            summary: None,
            args_json: None,
            output: Some("x".repeat(TOOL_OUTPUT_CHARS)),
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_stream: None,
            progress_bytes_total: 0,
            display: None,
            failure: None,
            permission: None,
            exit_code: None,
            completed_at_ms: None,
        },
    }
}

/// 遍历最终状态,累加可直接量度的保留字节
/// (工具输出/进度/思考文本/用户输入;markdown 内部无公共取字节数的口子,
/// 按 items 数粗估 —— 其真实文本量 ≈ 投喂的 delta 总量,由峰值断言兜底)。
fn retained_bytes(state: &SessionState) -> usize {
    let mut total = 0;
    for turn in &state.turns {
        total += turn.user_text.len();
        for round in &turn.rounds {
            for block in &round.blocks {
                match block {
                    Block::Text { md, .. } => total += md.items().len() * 32,
                    Block::Reasoning { raw, .. } => total += raw.len(),
                    Block::Tool { tool, progress, .. } => {
                        total += tool.output.as_deref().map(str::len).unwrap_or(0);
                        total += progress.len();
                    }
                    Block::Notice { text, .. } => total += text.len(),
                }
            }
        }
    }
    total
}

#[test]
fn long_context_soak_is_bounded() {
    let (deltas_per_turn, tool_every) = scenario();
    let mut state = SessionState::new("soak");

    // 基线差值测量:**不能**把 CURRENT 清零 —— harness 先于此测试分配的
    // 存活内存随后释放时会把计数打成假「下溢」(第一版就栽在这)。
    // PEAK 是高水位,清零安全;读数时减掉基线即得本段的净增长。
    let baseline = CURRENT.load(Ordering::Relaxed);
    PEAK.store(0, Ordering::Relaxed);
    let start = Instant::now();

    for turn in 0..TURNS {
        state.apply_entry(&entry(
            turn,
            TimelineEvent::TurnOpened {
                user_text: format!("问题 {turn}:请分析这段代码。"),
            },
        ));
        state.apply_entry(&entry(turn, text_block_open()));
        for delta in 0..deltas_per_turn {
            state.apply_entry(&entry(
                turn,
                TimelineEvent::TextDelta {
                    block_id: "text".into(),
                    fragment_seq: delta as u64,
                    delta: "结论片段。".repeat(2), // 10 字符/条
                },
            ));
        }
        state.apply_entry(&entry(
            turn,
            TimelineEvent::BlockSealed { block_id: "text".into() },
        ));
        if tool_every > 0 && turn % tool_every == 0 {
            state.apply_entry(&entry(turn, tool_block_open()));
            state.apply_entry(&entry(turn, tool_updated()));
        }
        state.apply_entry(&entry(
            turn,
            TimelineEvent::TurnSealed {
                state: TimelineTurnState::Completed,
                failure: None,
            },
        ));
        if turn % 1000 == 999 {
            eprintln!(
                "[probe] turn={} live={:.1} MB peak={:.1} MB",
                turn + 1,
                CURRENT.load(Ordering::Relaxed).saturating_sub(baseline) as f64 / 1048576.0,
                PEAK.load(Ordering::Relaxed).saturating_sub(baseline) as f64 / 1048576.0,
            );
        }
    }

    let elapsed = start.elapsed();
    let peak = PEAK
        .load(Ordering::Relaxed)
        .saturating_sub(baseline);
    let retained = retained_bytes(&state);

    // ── CPU:10 万级事件流必须在预算内跑完(本机实测 3–9s debug 构建;
    //    机器差异大,预算放宽到 60s,只抓「退化成线性扫描」级别的回归)。
    assert!(
        elapsed < Duration::from_secs(60),
        "reducer 耗时 {elapsed:?} 超预算 —— 事件定位/追加退化成线性扫描了?"
    );

    // ── 保留字节(稳态内存):500 工具 × 16 KiB 尾窗 ≈ 8.2 MB + 文本/结构
    //    ⇒ 有界 ≈ 10–12 MB;无界时仅工具输出就是 500 × 100 KB = 50 MB。
    assert!(
        retained < 16 * 1024 * 1024,
        "保留字节 {retained} 超过 16 MiB —— 某个字段的窗口截断失效了"
    );

    // ── 峰值分配(瞬态含事件构造):实测 ~40 MB(debug);无界 ≈ 90 MB+。
    assert!(
        peak < 64 * 1024 * 1024,
        "峰值分配 {peak} 超过 64 MiB —— 长上下文会顶爆内存"
    );

    // ── 规模与逐字段不变量(量级错了比崩溃更隐蔽)。
    assert_eq!(state.turns.len(), TURNS);
    if tool_every > 0 {
        for turn in state.turns.iter().step_by(tool_every) {
            for block in &turn.rounds[0].blocks {
                if let Block::Tool { tool, .. } = block {
                    let output = tool.output.as_deref().unwrap_or_default();
                    assert!(
                        output.chars().count() <= 16 * 1024 + 1,
                        "工具输出窗口截断失效:len={}",
                        output.chars().count()
                    );
                }
            }
        }
    }

    println!(
        "soak: {TURNS} turns / {} events / {elapsed:?} / peak {:.1} MB / retained {:.1} MB",
        TURNS * (deltas_per_turn + 6),
        peak as f64 / 1048576.0,
        retained as f64 / 1048576.0,
    );
}
