#![cfg(unix)]

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const EXIT_MARKER: &str = "__MERMAID_EXITED__:";
const INJECTED_MOUSE_REPORT: &[u8] = b"\x1b[<35;24;54M";

#[test]
fn double_ctrl_c_from_empty_tui_exits_and_restores_terminal_modes() {
    let binary = env!("CARGO_BIN_EXE_mermaid");
    let sandbox = test_sandbox("mermaid-pty-exit");
    let home = sandbox.join("home");
    let config = sandbox.join("config");
    let project = sandbox.join("project");
    std::fs::create_dir_all(&home).expect("create test home");
    std::fs::create_dir_all(&config).expect("create test config");
    std::fs::create_dir_all(&project).expect("create test project");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let output = Arc::new(Mutex::new(Vec::new()));
    let output_for_reader = Arc::clone(&output);
    let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
    let (reader_done_tx, reader_done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => output_for_reader
                    .lock()
                    .expect("output lock")
                    .extend(&buf[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = reader_done_tx.send(());
    });

    let script = format!(
        "stty sane; {} --model anthropic/pty-exit-test; status=$?; printf '\\n{}%s\\n' \"$status\"; stty -a",
        shell_quote(binary),
        EXIT_MARKER,
    );
    let mut cmd = CommandBuilder::new("bash");
    cmd.args(["--noprofile", "--norc", "-lc", &script]);
    cmd.cwd(project.as_os_str());
    cmd.env("HOME", home.as_os_str());
    cmd.env("XDG_CONFIG_HOME", config.as_os_str());
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_BACKTRACE", "0");

    let mut child = pair.slave.spawn_command(cmd).expect("spawn shell in pty");
    drop(pair.slave);

    assert!(
        wait_for_output(
            &output,
            |bytes| bytes.windows(8).any(|w| w == b"\x1b[?1049h"),
            Duration::from_secs(3)
        ),
        "Mermaid did not enter the alternate screen. Output:\n{}",
        output_text(&output)
    );

    let mut writer = pair.master.take_writer().expect("take pty writer");
    writer
        .write_all(INJECTED_MOUSE_REPORT)
        .expect("write injected mouse report");
    writer.flush().expect("flush mouse report");
    std::thread::sleep(Duration::from_millis(50));
    // Press-twice-to-exit: the first Ctrl+C arms the confirm window, the
    // second (inside the 3s window) exits. The gap must be long enough that
    // the two bytes can't coalesce into one read burst, short enough to stay
    // inside the window even on a slow runner.
    writer.write_all(&[0x03]).expect("write first Ctrl+C");
    writer.flush().expect("flush first Ctrl+C");
    std::thread::sleep(Duration::from_millis(150));
    writer.write_all(&[0x03]).expect("write second Ctrl+C");
    writer.flush().expect("flush second Ctrl+C");

    let status = wait_for_child(&mut child, Duration::from_secs(8))
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!(
                "Mermaid did not exit after double Ctrl+C. Output:\n{}",
                output_text(&output)
            );
        })
        .expect("wait for child");
    drop(writer);

    let _ = reader_done_rx.recv_timeout(Duration::from_secs(2));
    let bytes = output.lock().expect("output lock").clone();
    let text = String::from_utf8_lossy(&bytes);

    assert!(
        status.success(),
        "shell wrapper exited unsuccessfully: {:?}\n{}",
        status,
        text
    );
    assert!(
        text.contains(&format!("{}0", EXIT_MARKER)),
        "Mermaid did not report a clean exit. Output:\n{}",
        text
    );

    for cleanup in [
        b"\x1b[?1000l".as_slice(),
        b"\x1b[?1002l".as_slice(),
        b"\x1b[?1003l".as_slice(),
        b"\x1b[?1006l".as_slice(),
        b"\x1b[?2004l".as_slice(),
        b"\x1b[?1049l".as_slice(),
    ] {
        assert!(
            bytes.windows(cleanup.len()).any(|window| window == cleanup),
            "missing cleanup sequence {:?}. Output:\n{}",
            String::from_utf8_lossy(cleanup),
            text
        );
    }

    assert!(
        !bytes
            .windows(INJECTED_MOUSE_REPORT.len())
            .any(|window| window == INJECTED_MOUSE_REPORT),
        "injected SGR mouse report leaked back into shell output:\n{}",
        text
    );

    let after_marker = text
        .split_once(EXIT_MARKER)
        .map(|(_, after)| after)
        .unwrap_or_default();
    assert!(
        terminal_mode_token_present(after_marker, "icanon")
            && terminal_mode_token_present(after_marker, "echo")
            && !terminal_mode_token_present(after_marker, "-icanon")
            && !terminal_mode_token_present(after_marker, "-echo"),
        "terminal did not return to canonical echo mode after Mermaid exit:\n{}",
        text
    );
}

/// Regression test for the full-repaint path (`full_redraw_seq` → run.rs).
///
/// `Terminal::clear()` snapshots the cursor with an ESC[6n query. Under a pty
/// harness nothing ever answers that query, so the app died on crossterm's 2s
/// deadline ("The cursor position could not be read within a normal
/// duration") — and in a real terminal the same death fired whenever the
/// repaint was triggered from the effect arm (shell tool finished) because
/// the `EventStream` reader thread holds crossterm's internal reader mutex
/// while idle and swallows the reply. The repaint must not query the
/// terminal at all: Ctrl+L here must neither emit ESC[6n nor kill the app.
#[test]
fn ctrl_l_full_repaint_does_not_query_cursor_or_crash() {
    let binary = env!("CARGO_BIN_EXE_mermaid");
    let sandbox = test_sandbox("mermaid-pty-repaint");
    let home = sandbox.join("home");
    let config = sandbox.join("config");
    let project = sandbox.join("project");
    std::fs::create_dir_all(&home).expect("create test home");
    std::fs::create_dir_all(&config).expect("create test config");
    std::fs::create_dir_all(&project).expect("create test project");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let output = Arc::new(Mutex::new(Vec::new()));
    let output_for_reader = Arc::clone(&output);
    let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
    let (reader_done_tx, reader_done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => output_for_reader
                    .lock()
                    .expect("output lock")
                    .extend(&buf[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = reader_done_tx.send(());
    });

    let script = format!(
        "stty sane; {} --model anthropic/pty-repaint-test; status=$?; printf '\\n{}%s\\n' \"$status\"",
        shell_quote(binary),
        EXIT_MARKER,
    );
    let mut cmd = CommandBuilder::new("bash");
    cmd.args(["--noprofile", "--norc", "-lc", &script]);
    cmd.cwd(project.as_os_str());
    cmd.env("HOME", home.as_os_str());
    cmd.env("XDG_CONFIG_HOME", config.as_os_str());
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_BACKTRACE", "0");

    let mut child = pair.slave.spawn_command(cmd).expect("spawn shell in pty");
    drop(pair.slave);

    assert!(
        wait_for_output(
            &output,
            |bytes| bytes.windows(8).any(|w| w == b"\x1b[?1049h"),
            Duration::from_secs(3)
        ),
        "Mermaid did not enter the alternate screen. Output:\n{}",
        output_text(&output)
    );

    // Let the startup kitty-protocol probe expire (2s, unanswered in a pty)
    // so Ctrl+L is handled by the steady-state main loop, matching the
    // real-world trigger timing.
    std::thread::sleep(Duration::from_millis(2500));

    let mut writer = pair.master.take_writer().expect("take pty writer");
    writer.write_all(&[0x0C]).expect("write Ctrl+L");
    writer.flush().expect("flush Ctrl+L");

    // Outlive crossterm's 2s cursor-position deadline: the buggy repaint
    // path dies here; the fixed one repaints and keeps running.
    std::thread::sleep(Duration::from_millis(3000));

    writer.write_all(&[0x03]).expect("write first Ctrl+C");
    writer.flush().expect("flush first Ctrl+C");
    std::thread::sleep(Duration::from_millis(150));
    writer.write_all(&[0x03]).expect("write second Ctrl+C");
    writer.flush().expect("flush second Ctrl+C");

    let status = wait_for_child(&mut child, Duration::from_secs(8))
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!(
                "Mermaid did not exit after double Ctrl+C. Output:\n{}",
                output_text(&output)
            );
        })
        .expect("wait for child");
    drop(writer);

    let _ = reader_done_rx.recv_timeout(Duration::from_secs(2));
    let bytes = output.lock().expect("output lock").clone();
    let text = String::from_utf8_lossy(&bytes);

    assert!(
        status.success(),
        "shell wrapper exited unsuccessfully: {:?}\n{}",
        status,
        text
    );
    assert!(
        text.contains(&format!("{}0", EXIT_MARKER)),
        "Mermaid did not survive the Ctrl+L full repaint (expected clean exit \
         via double Ctrl+C afterwards). Output:\n{}",
        text
    );

    const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
    assert!(
        !bytes
            .windows(CURSOR_POSITION_QUERY.len())
            .any(|window| window == CURSOR_POSITION_QUERY),
        "full-repaint path emitted an ESC[6n cursor-position query; it must \
         not query the terminal (the reply deadlocks against the EventStream \
         reader). Output:\n{}",
        text
    );
}

fn wait_for_output(
    output: &Arc<Mutex<Vec<u8>>>,
    predicate: impl Fn(&[u8]) -> bool,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate(&output.lock().expect("output lock")) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn wait_for_child(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    timeout: Duration,
) -> Option<std::io::Result<portable_pty::ExitStatus>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Some(Ok(status)),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Some(Err(error)),
        }
    }
    None
}

fn terminal_mode_token_present(stty: &str, token: &str) -> bool {
    stty.split(|c: char| c.is_whitespace() || c == ';')
        .any(|part| part == token)
}

fn output_text(output: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&output.lock().expect("output lock")).into_owned()
}

fn test_sandbox(prefix: &str) -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{}-{}-{}", prefix, std::process::id(), nonce))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
