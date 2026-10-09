use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use mermaid_model::ids::{ToolCallId, TurnId};
use mermaid_runtime::SafetyMode;
use serde_json::json;

use super::desktop::fake::{Event, FakeDesktop};
use super::*;

fn ctx(mode: SafetyMode) -> ExecContext {
    let mut config = mermaid_domain::Config::default();
    config.safety.mode = mode;
    crate::providers::ctx::test_exec_context_with_config(
        TurnId(1),
        ToolCallId(1),
        PathBuf::from("."),
        config,
    )
    .0
}

fn act(
    desktop: &mut FakeDesktop,
    shot: &mut Option<Shot>,
    args: serde_json::Value,
) -> Result<Done, String> {
    run(desktop, shot, &parse(&args)?)
}

fn decoded_size(png: &[u8]) -> (u32, u32) {
    image::load_from_memory(png)
        .expect("a PNG")
        .to_rgb8()
        .dimensions()
}

#[test]
fn a_picture_is_fitted_to_what_every_model_takes() {
    assert_eq!(fit(1920, 1080), (1429, 804));
    assert_eq!(
        fit(1280, 800),
        (1280, 800),
        "a small screen is sent as it is"
    );
    assert_eq!(fit(3840, 2160), (1429, 804));
    assert_eq!(
        fit(3136, 500),
        (1568, 250),
        "the long edge binds a wide strip"
    );
    for (w, h) in [(1920, 1080), (2560, 1600), (5120, 2880), (800, 600)] {
        let (fw, fh) = fit(w, h);
        assert!(fw <= MAX_EDGE && fh <= MAX_EDGE);
        assert!(u64::from(fw) * u64::from(fh) <= MAX_PIXELS);
    }
}

#[test]
fn a_screenshot_comes_back_at_the_fitted_size() {
    let mut desktop = FakeDesktop::new((1920, 1080), (1920, 1080));
    let mut shot = None;
    let done = act(&mut desktop, &mut shot, json!({"action": "screenshot"})).unwrap();
    assert_eq!(decoded_size(done.png.as_deref().unwrap()), (1429, 804));
    assert!(done.text.contains("1429x804"), "{}", done.text);
    assert_eq!(shot.unwrap().sent, (1429, 804));
}

#[test]
fn coordinates_are_scaled_from_the_screenshot_to_the_screen() {
    // A Retina Mac: the capture is twice the size of the input space.
    let mut desktop = FakeDesktop::new((2880, 1800), (1440, 900));
    let mut shot = None;
    act(&mut desktop, &mut shot, json!({"action": "screenshot"})).unwrap();
    let sent = shot.unwrap().sent;
    assert_eq!(sent, (1356, 847));
    act(
        &mut desktop,
        &mut shot,
        json!({"action": "left_click", "coordinate": [678, 423]}),
    )
    .unwrap();
    assert_eq!(
        desktop.events,
        vec![
            Event::Move(720, 449),
            Event::Button(Button::Left, true),
            Event::Button(Button::Left, false),
        ]
    );
    desktop.cursor = (1440 - 1, 900 - 1);
    let done = act(
        &mut desktop,
        &mut shot,
        json!({"action": "cursor_position"}),
    )
    .unwrap();
    assert_eq!(done.text, "X=1355, Y=846");
}

#[test]
fn a_coordinate_off_the_screen_is_refused_before_anything_moves() {
    let mut desktop = FakeDesktop::new((1920, 1080), (1920, 1080));
    let mut shot = None;
    let err = act(
        &mut desktop,
        &mut shot,
        json!({"action": "left_click", "coordinate": [1429, 10]}),
    )
    .unwrap_err();
    assert_eq!(
        err,
        "coordinate (1429, 10) is off the screen, which is 1429x804 in screenshot pixels"
    );
    assert!(desktop.events.is_empty());
}

#[test]
fn keys_held_for_a_click_are_released_in_reverse() {
    let mut desktop = FakeDesktop::new((1000, 500), (1000, 500));
    let mut shot = None;
    act(
        &mut desktop,
        &mut shot,
        json!({"action": "double_click", "text": "ctrl+shift"}),
    )
    .unwrap();
    assert_eq!(
        desktop.events,
        vec![
            Event::Key(Key::Control, true),
            Event::Key(Key::Shift, true),
            Event::Button(Button::Left, true),
            Event::Button(Button::Left, false),
            Event::Button(Button::Left, true),
            Event::Button(Button::Left, false),
            Event::Key(Key::Shift, false),
            Event::Key(Key::Control, false),
        ]
    );
}

#[test]
fn key_type_scroll_and_drag_send_what_the_model_asked() {
    let mut desktop = FakeDesktop::new((1000, 500), (1000, 500));
    let mut shot = None;
    act(
        &mut desktop,
        &mut shot,
        json!({"action": "key", "text": "ctrl+s", "repeat": 2}),
    )
    .unwrap();
    act(
        &mut desktop,
        &mut shot,
        json!({"action": "type", "text": "héllo"}),
    )
    .unwrap();
    act(
        &mut desktop,
        &mut shot,
        json!({"action": "scroll", "coordinate": [10, 20], "scroll_direction": "up", "scroll_amount": 5}),
    )
    .unwrap();
    act(
        &mut desktop,
        &mut shot,
        json!({"action": "left_click_drag", "start_coordinate": [1, 2], "coordinate": [3, 4]}),
    )
    .unwrap();
    let chord = [
        Event::Key(Key::Control, true),
        Event::Key(Key::Char('s'), true),
        Event::Key(Key::Char('s'), false),
        Event::Key(Key::Control, false),
    ];
    let mut expected: Vec<Event> = chord.iter().chain(chord.iter()).cloned().collect();
    expected.extend([
        Event::Text("héllo".to_string()),
        Event::Move(10, 20),
        Event::Scroll(0, -5),
        Event::Move(1, 2),
        Event::Button(Button::Left, true),
        Event::Move(3, 4),
        Event::Button(Button::Left, false),
    ]);
    assert_eq!(desktop.events, expected);
}

#[test]
fn zoom_returns_the_region_at_capture_resolution() {
    // The capture is twice the sent size, so a 100x50 region holds 200x100
    // real pixels.
    let mut desktop = FakeDesktop::new((3136, 1000), (3136, 1000));
    let mut shot = None;
    let done = act(
        &mut desktop,
        &mut shot,
        json!({"action": "zoom", "region": [100, 50, 200, 100]}),
    )
    .unwrap();
    let png = done.png.unwrap();
    assert_eq!(decoded_size(&png), (200, 100));
    let picture = image::load_from_memory(&png).unwrap().to_rgb8();
    // The fake screen's red channel is x % 256: the crop starts at x = 200.
    assert_eq!(picture.get_pixel(0, 0).0[0], 200);
    let err = act(
        &mut desktop,
        &mut shot,
        json!({"action": "zoom", "region": [10, 10, 5, 20]}),
    )
    .unwrap_err();
    assert!(err.contains("x1 > x0"), "{err}");
}

#[test]
fn bad_arguments_teach_the_fix() {
    for (args, want) in [
        (json!({}), "`action` is required"),
        (
            json!({"action": "teleport"}),
            "unknown action \"teleport\"; the actions are screenshot, zoom",
        ),
        (json!({"action": "type"}), "`type` needs `text`"),
        (
            json!({"action": "mouse_move"}),
            "`mouse_move` needs `coordinate`, [x, y]",
        ),
        (
            json!({"action": "scroll"}),
            "`scroll` needs `scroll_direction`",
        ),
        (
            json!({"action": "wait", "duration": 500}),
            "`duration` must be from 0 to 100 seconds",
        ),
        (
            json!({"action": "left_click", "coordinate": [1]}),
            "`coordinate` must be two numbers",
        ),
        (
            json!({"action": "key", "text": "ctrl+hyper"}),
            "unknown key \"hyper\"",
        ),
    ] {
        let err = parse(&args).unwrap_err();
        assert!(err.starts_with(want), "{args}: {err}");
    }
}

#[test]
fn every_toolset_member_is_an_action() {
    for member in MEMBERS {
        let err = parse(&json!({"action": member})).err().unwrap_or_default();
        assert!(!err.starts_with("unknown action"), "{member}: {err}");
    }
}

fn fake_screen() -> Result<Box<dyn Desktop>, String> {
    Ok(Box::new(FakeDesktop::new((1920, 1080), (1920, 1080))))
}

fn no_screen() -> Result<Box<dyn Desktop>, String> {
    Err("the screen was opened".to_string())
}

#[tokio::test]
async fn read_only_takes_screenshots_and_blocks_input() {
    let tool = ComputerTool::new();
    let ctx = ctx(SafetyMode::ReadOnly);
    let outcome = tool
        .execute_with(json!({"action": "screenshot"}), &ctx, fake_screen)
        .await;
    assert!(outcome.is_success(), "{outcome:?}");
    let images = outcome.images().expect("the screenshot");
    let png = base64::engine::general_purpose::STANDARD
        .decode(&images[0])
        .unwrap();
    assert_eq!(decoded_size(&png), (1429, 804));

    let outcome = tool
        .execute_with(
            json!({"action": "left_click", "coordinate": [5, 5]}),
            &ctx,
            no_screen,
        )
        .await;
    assert!(!outcome.is_success());
    let error = outcome.error_message().unwrap_or_default();
    assert!(
        !error.contains("the screen was opened"),
        "the gate refuses before the screen is touched: {error}"
    );
}

#[tokio::test]
async fn full_access_runs_input() {
    let tool = ComputerTool::new();
    let outcome = tool
        .execute_with(
            json!({"action": "type", "text": "hi"}),
            &ctx(SafetyMode::FullAccess),
            fake_screen,
        )
        .await;
    assert!(outcome.is_success(), "{outcome:?}");
    assert_eq!(outcome.output(), "Typed 2 characters.");
}

#[test]
fn the_approval_prompt_names_the_action_and_its_target() {
    assert_eq!(
        describe(&json!({"action": "left_click", "coordinate": [10, 20], "text": "shift"})),
        "computer left_click (10, 20) \"shift\""
    );
    assert_eq!(
        describe(&json!({"action": "scroll", "coordinate": [1, 2], "scroll_direction": "down"})),
        "computer scroll (1, 2) down"
    );
}

#[test]
fn a_batch_gate_lists_every_action() {
    let click = json!({"action": "left_click", "coordinate": [10, 20]});
    let typing = json!({"action": "type", "text": "hello"});
    assert_eq!(
        gate_summary(&click, &[click.clone()]),
        "computer left_click (10, 20)"
    );
    assert_eq!(
        gate_summary(&click, &[click.clone(), typing]),
        "computer, 2 actions: left_click (10, 20); type \"hello\""
    );
}

fn run_key(messages: usize) -> RunKey {
    RunKey {
        session: Some("s".to_string()),
        messages,
        latest: Some("click it".to_string()),
    }
}

#[test]
fn moving_the_mouse_stops_input_until_the_next_message() {
    let mut desktop = FakeDesktop::new((1000, 500), (1000, 500));
    let mut state = ToolState::default();
    let click = |x: i64| parse(&json!({"action": "left_click", "coordinate": [x, 10]})).unwrap();
    let screenshot = parse(&json!({"action": "screenshot"})).unwrap();

    run_watched(&mut desktop, &mut state, &run_key(1), &click(10)).unwrap();
    // A small jitter is not the user.
    desktop.cursor = (12, 8);
    run_watched(&mut desktop, &mut state, &run_key(1), &click(20)).unwrap();

    desktop.cursor = (400, 300);
    desktop.events.clear();
    let err = run_watched(&mut desktop, &mut state, &run_key(1), &click(30)).unwrap_err();
    assert_eq!(err, MOVED);
    assert!(desktop.events.is_empty(), "nothing was sent after the move");
    assert!(state.watch.as_ref().unwrap().stopped);
    assert!(
        run_watched(&mut desktop, &mut state, &run_key(1), &screenshot).is_ok(),
        "looking at the screen is not input"
    );

    // The user's next message starts a new run.
    run_watched(&mut desktop, &mut state, &run_key(2), &click(30)).unwrap();
    assert_eq!(desktop.events.first(), Some(&Event::Move(30, 10)));
}

#[tokio::test(start_paused = true)]
async fn a_stopped_run_refuses_input_without_touching_the_screen() {
    let tool = ComputerTool::new();
    let ctx = ctx(SafetyMode::FullAccess);
    tool.state().watch = Some(Watch {
        run: RunKey::of(&ctx),
        pointer: None,
        stopped: true,
    });
    let outcome = tool
        .execute_with(json!({"action": "key", "text": "Return"}), &ctx, no_screen)
        .await;
    assert_eq!(outcome.error_message(), Some(MOVED));
    let outcome = tool
        .execute_with(json!({"action": "screenshot"}), &ctx, fake_screen)
        .await;
    assert!(outcome.is_success(), "{outcome:?}");
}

/// Allows every action and keeps what it was asked.
#[derive(Default)]
struct Recorder(Mutex<Vec<crate::providers::VetRequest>>);

#[async_trait::async_trait]
impl crate::providers::AutoClassifier for Recorder {
    async fn vet(&self, req: &crate::providers::VetRequest) -> crate::providers::VetVerdict {
        self.0.lock().unwrap().push(req.clone());
        crate::providers::VetVerdict::allow()
    }
}

#[tokio::test(start_paused = true)]
async fn auto_checks_a_batch_once_with_every_action_and_the_screen() {
    let tool = ComputerTool::new();
    let recorder = Arc::new(Recorder::default());
    // Each call opens a fresh fake screen with the pointer at (0, 0), so the
    // click leaves it there and the mouse watch sees no move.
    let click = json!({"action": "left_click", "coordinate": [0, 0]});
    let typing = json!({"action": "type", "text": "hello"});
    let mut ctx = ctx(SafetyMode::Auto);
    ctx.goal = mermaid_domain::UserGoal::from_request("type hello in the box");
    ctx.classifier = Some(recorder.clone());

    tool.execute_with(json!({"action": "screenshot"}), &ctx, fake_screen)
        .await;
    ctx.computer_batch = vec![click.clone(), typing.clone()];
    for args in [&click, &typing] {
        let outcome = tool.execute_with(args.clone(), &ctx, fake_screen).await;
        assert!(outcome.is_success(), "{outcome:?}");
    }
    {
        let asked = recorder.0.lock().unwrap();
        assert_eq!(asked.len(), 1, "one check for the whole batch");
        assert_eq!(
            asked[0].arguments,
            Some(json!({"actions": [click.clone(), typing.clone()]}))
        );
        let screen = asked[0].screen.as_deref().expect("the last screenshot");
        let png = base64::engine::general_purpose::STANDARD
            .decode(screen)
            .unwrap();
        assert_eq!(decoded_size(&png), (1429, 804));
    }

    // The next model response is a new batch, so it is checked again.
    ctx.turn = TurnId(2);
    ctx.computer_batch = vec![click.clone()];
    tool.execute_with(click.clone(), &ctx, fake_screen).await;
    let asked = recorder.0.lock().unwrap();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[1].arguments, Some(click));
}

#[tokio::test(start_paused = true)]
async fn a_headless_run_asks_by_failing_closed_unless_trusted() {
    let tool = ComputerTool::new();
    let ctx = ctx(SafetyMode::Ask);
    assert!(ctx.approval.is_none(), "a headless run has no prompt");
    let click = json!({"action": "left_click", "coordinate": [5, 5]});
    let outcome = tool.execute_with(click.clone(), &ctx, no_screen).await;
    let error = outcome.error_message().unwrap_or_default();
    assert!(error.contains("headless run"), "{error}");

    let mut config = mermaid_domain::Config::default();
    config.safety.mode = SafetyMode::Ask;
    config.safety.allow_untrusted_headless_tools = true;
    let (ctx, _rx) = crate::providers::ctx::test_exec_context_with_config(
        TurnId(1),
        ToolCallId(1),
        PathBuf::from("."),
        config,
    );
    let outcome = tool.execute_with(click, &ctx, fake_screen).await;
    assert!(outcome.is_success(), "{outcome:?}");
}
