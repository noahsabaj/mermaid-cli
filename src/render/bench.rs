//! Idle-frame cost harness. NOT a correctness suite — an `#[ignore]`d
//! measurement rig kept in-tree so the numbers behind render-path decisions
//! stay reproducible and re-checkable.
//!
//! Run with:
//! ```text
//! cargo test --release --lib render::bench -- --ignored --nocapture
//! ```
//!
//! `--release` is not optional: this crate builds with `lto = true`, and a
//! debug build measures a different program.
//!
//! What it models: the real loop in `src/app/run.rs` draws at the top of every
//! iteration and one `select!` arm is a 16ms `interval`, so an idle session
//! repaints ~60x/second forever. The loop reuses ONE `Terminal` and ONE
//! `RenderCache` across frames, so this harness does too — otherwise the
//! wrapped-line cache would be cold every frame and the numbers would describe
//! a situation that never happens.

use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::{RenderCache, render};
use crate::app::Config;
use crate::domain::State;
use crate::models::{ChatMessage, ChatMessageKind};

/// Frames discarded before timing starts (cache fill, allocator warm-up).
const WARMUP: usize = 60;
/// Timed frames per configuration.
const SAMPLES: usize = 300;
/// Terminal size — the roomy end of the snapshot suite's two sizes.
const SIZE: (u16, u16) = (120, 40);

fn fixed_now() -> chrono::DateTime<chrono::Local> {
    chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05+00:00")
        .expect("fixture timestamp parses")
        .with_timezone(&chrono::Local)
}

/// Roughly one realistic assistant turn: prose plus a fenced block, so the
/// markdown path and the wrapper both do representative work.
fn body(i: usize) -> String {
    format!(
        "Turn {i}: the classifier splits on unquoted control operators, so a chained \
         command cannot hide behind a benign head. Worth checking the redirect forms \
         separately.\n\n```rust\nfn seg_{i}(cmd: &str) -> usize {{\n    cmd.len() + {i}\n}}\n```\n\n\
         That covers the common shapes; the rest fall through to the worst-segment rule."
    )
}

/// A transcript of `pairs` user/assistant exchanges.
///
/// `marker` inserts a persistent `ContextMarker` (what a plan toggle leaves
/// behind). `continuation` marks the final assistant message as an
/// auto-continue half, which is the only thing that still forces the stitch.
fn state_with(pairs: usize, marker: bool, continuation: bool) -> State {
    let mut state = State::new(
        Config::default(),
        std::path::PathBuf::from("/project/demo"),
        "ollama/test".to_string(),
        fixed_now(),
    );
    let now = fixed_now();
    for i in 0..pairs {
        state
            .session
            .append(ChatMessage::user(format!("question {i}")), now);
        state.session.append(ChatMessage::assistant(body(i)), now);
    }
    if marker {
        let mut m = ChatMessage::system("Plan mode is now ON. Author the plan at x.md.");
        m.kind = ChatMessageKind::ContextMarker;
        state.session.append(m, now);
    }
    if continuation {
        let mut m = ChatMessage::assistant(body(pairs + 1));
        m.kind = ChatMessageKind::Continuation;
        state.session.append(m, now);
    }
    state
}

struct Stats {
    mean_us: f64,
    sd_us: f64,
    max_us: f64,
}

fn summarize(samples: &[Duration]) -> Stats {
    let us: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1e6).collect();
    let n = us.len() as f64;
    let mean = us.iter().sum::<f64>() / n;
    let var = us.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    Stats {
        mean_us: mean,
        sd_us: var.sqrt(),
        max_us: us.iter().cloned().fold(0.0, f64::max),
    }
}

/// Time `SAMPLES` idle frames, reusing one terminal + one cache throughout.
fn time_idle_frames(state: &State) -> Stats {
    let backend = TestBackend::new(SIZE.0, SIZE.1);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut cache = RenderCache::new();
    cache.hostname = "benchhost".to_string();
    cache.username = "benchuser".to_string();

    for _ in 0..WARMUP {
        terminal
            .draw(|f| render(state, &mut cache, f))
            .expect("draw");
    }
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let t0 = Instant::now();
        terminal
            .draw(|f| render(state, &mut cache, f))
            .expect("draw");
        samples.push(t0.elapsed());
    }
    summarize(&samples)
}

fn transcript_bytes(state: &State) -> usize {
    state
        .session
        .messages()
        .iter()
        .map(|m| m.content.len())
        .sum()
}

#[test]
#[ignore = "measurement harness; run with --release --ignored --nocapture"]
fn idle_frame_cost_by_transcript_size() {
    println!(
        "\n{:>6}  {:>9}  {:>12}  {:>10}  {:>9}  {:>9}  {:>9}",
        "pairs", "msgs", "bytes", "variant", "mean us", "sd us", "max us"
    );
    for pairs in [25usize, 100, 400, 1000] {
        for (label, marker, continuation) in [
            ("plain", false, false),
            ("marker", true, false),
            ("continue", false, true),
        ] {
            let state = state_with(pairs, marker, continuation);
            let s = time_idle_frames(&state);
            println!(
                "{:>6}  {:>9}  {:>12}  {:>10}  {:>9.1}  {:>9.1}  {:>9.1}",
                pairs,
                state.session.messages().len(),
                transcript_bytes(&state),
                label,
                s.mean_us,
                s.sd_us,
                s.max_us,
            );
        }
    }
    println!();
}

/// Attribute the frame: how much of it is the chat transcript widget alone?
/// Rendered directly into a buffer, reusing the warmed wrapped-line cache, so
/// this is steady-state cost and not first-paint parsing.
#[test]
#[ignore = "measurement harness; run with --release --ignored --nocapture"]
fn chat_widget_share_of_the_frame() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::StatefulWidget;

    use super::widgets::ChatWidget;

    println!(
        "\n{:>6}  {:>12}  {:>12}  {:>12}  {:>8}",
        "pairs", "bytes", "frame us", "chat us", "share"
    );
    for pairs in [25usize, 100, 400, 1000] {
        let state = state_with(pairs, true, false);
        let frame = time_idle_frames(&state);

        let mut cache = RenderCache::new();
        let area = Rect::new(0, 0, SIZE.0, SIZE.1);
        let mut buf = Buffer::empty(area);
        let messages = state.session.messages();

        let run = |cache: &mut RenderCache, buf: &mut Buffer| {
            let w = ChatWidget {
                messages,
                // The bench renders the widget standalone, so it stands in for
                // `render::chat_content_key`. Constant: the transcript does not
                // change across frames, which is the idle case being measured.
                content_key: 1,
                theme: &cache.theme,
                wrapped_line_cache: &mut cache.wrapped_line_cache,
                show_reasoning: false,
                blink_on: false,
            };
            // SAFETY-free split borrow: chat state is a separate field.
            let mut chat = std::mem::take(&mut cache.chat);
            w.render(area, buf, &mut chat);
            cache.chat = chat;
        };

        for _ in 0..WARMUP {
            run(&mut cache, &mut buf);
        }
        let mut samples = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let t0 = Instant::now();
            run(&mut cache, &mut buf);
            samples.push(t0.elapsed());
        }
        let chat = summarize(&samples);
        println!(
            "{:>6}  {:>12}  {:>12.1}  {:>12.1}  {:>7.0}%",
            pairs,
            transcript_bytes(&state),
            frame.mean_us,
            chat.mean_us,
            100.0 * chat.mean_us / frame.mean_us,
        );
    }
    println!();
}

/// Isolate the two stitch components from everything else in the frame, so
/// their share of the idle frame is attributable rather than inferred.
#[test]
#[ignore = "measurement harness; run with --release --ignored --nocapture"]
fn stitch_component_cost() {
    println!(
        "\n{:>6}  {:>12}  {:>14}  {:>14}",
        "pairs", "bytes", "fingerprint us", "stitch us"
    );
    for pairs in [25usize, 100, 400, 1000] {
        let state = state_with(pairs, true, false);
        let committed = state.session.messages();

        for _ in 0..WARMUP {
            std::hint::black_box(super::stitch_fingerprint(committed));
        }
        let mut fp = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let t0 = Instant::now();
            std::hint::black_box(super::stitch_fingerprint(committed));
            fp.push(t0.elapsed());
        }

        let mut st = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let t0 = Instant::now();
            std::hint::black_box(super::stitch_committed(committed));
            st.push(t0.elapsed());
        }

        println!(
            "{:>6}  {:>12}  {:>14.1}  {:>14.1}",
            pairs,
            transcript_bytes(&state),
            summarize(&fp).mean_us,
            summarize(&st).mean_us,
        );
    }
    println!();
}
