//! SIGHUP config reload for `txwatch watch`. Closes #98.
//!
//! SIGHUP is a Unix-only signal (the CLI installs a no-op handler on other
//! platforms), so this whole test binary is gated to Unix and is skipped on
//! Windows. macOS is Unix and does run it.
#![cfg(unix)]

use std::{
    env, fs,
    io::{BufRead, BufReader},
    net::TcpListener,
    process::{Child, Command, Stdio},
    sync::mpsc::{channel, Receiver},
    thread,
    time::{Duration, Instant},
};

/// A contract on a custom network whose Horizon is never reachable, so the
/// watcher runs without touching the internet (polls just log an error).
fn contract(label: &str, contract_id: &str) -> String {
    format!(
        r#"
[[contracts]]
label       = "{label}"
contract_id = "{contract_id}"
network     = {{ horizon_url = "http://127.0.0.1:1" }}
webhook_url = "https://hooks.example.com/test"

  [[contracts.rules]]
  type = "AnyTransaction"
"#
    )
}

const ALPHA: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";
const BETA: &str = "CAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC526";

fn wait_for(lines: &Receiver<String>, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match lines.recv_timeout(remaining) {
            Ok(line) if line.contains(needle) => return line,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    panic!("timed out waiting for log line containing {:?}", needle);
}

fn sighup(child: &Child) {
    let status = Command::new("kill")
        .args(["-HUP", &child.id().to_string()])
        .status()
        .expect("failed to run kill");
    assert!(status.success());
}

/// A contract whose Horizon is a local mock server, so the watcher actually
/// sees transactions and can fire alerts.
fn live_contract(label: &str, contract_id: &str, horizon_url: &str) -> String {
    format!(
        r#"
[[contracts]]
label       = "{label}"
contract_id = "{contract_id}"
network     = {{ horizon_url = "{horizon_url}" }}
webhook_url = "https://hooks.example.com/test"

  [[contracts.rules]]
  type = "AnyTransaction"
"#
    )
}

/// Spawns a tiny HTTP server that serves a fixed Horizon transactions page.
/// Returns the base URL and a handle that keeps the server alive.
fn mock_horizon(contract_id: &str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock horizon");
    let addr = listener.local_addr().unwrap();
    let body = format!(
        r#"{{"_embedded":{{"records":[{{"id":"tx1","hash":"deadbeef","ledger":1,"created_at":"2024-01-01T00:00:00Z","successful":true,"source_account":"GAAA","contract_id":"{contract_id}"}}]}}}}"#
    );
    let handle = thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => break,
            };
            // Read the request line (and headers) then respond.
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                line.clear();
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            use std::io::Write;
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{}", addr), handle)
}

#[test]
fn watch_reloads_config_on_sighup_and_keeps_old_config_when_invalid() {
    // Unique per-process file name so concurrent test runs (or a stale file
    // from a previous run) can't interfere, and so the path is portable.
    let path = env::temp_dir().join(format!(
        "txwatch_sighup_reload_{}.toml",
        std::process::id()
    ));
    fs::write(
        &path,
        format!("poll_interval_seconds = 10\n{}", contract("Alpha", ALPHA)),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args(["--config", path.to_str().unwrap(), "watch", "--dry-run"])
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start txwatch");

    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });

    wait_for(&lines, "TxWatch polling engine started");

    // Invalid file: reload is rejected and the old config keeps running.
    fs::write(
        &path,
        format!("poll_interval_seconds = 1\n{}", contract("Alpha", ALPHA)),
    )
    .unwrap();
    sighup(&child);
    let failed = wait_for(&lines, "config reload failed");
    assert!(
        failed.contains("poll_interval_seconds must be >= 5"),
        "{}",
        failed
    );

    // Valid file with an extra contract: reload is applied.
    fs::write(
        &path,
        format!(
            "poll_interval_seconds = 10\n{}{}",
            contract("Alpha", ALPHA),
            contract("Beta", BETA)
        ),
    )
    .unwrap();
    sighup(&child);
    let reloaded = wait_for(&lines, "configuration reloaded");
    assert!(reloaded.contains("contracts=2"), "{}", reloaded);
    assert!(
        child.try_wait().unwrap().is_none(),
        "watcher must still be running"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(&path);
}

#[test]
fn watch_reload_keeps_horizon_url_override() {
    let path = env::temp_dir().join("txwatch_sighup_horizon_override.toml");
    fs::write(
        &path,
        format!("poll_interval_seconds = 10\n{}", contract("Alpha", ALPHA)),
    )
    .unwrap();

    let override_url = "http://127.0.0.1:1";
    let mut child = Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args([
            "--config",
            path.to_str().unwrap(),
            "--horizon-url",
            override_url,
            "watch",
            "--dry-run",
        ])
fn watch_reload_preserves_cooldowns() {
    let (horizon_url, _server) = mock_horizon(ALPHA);
    let path = env::temp_dir().join("txwatch_sighup_cooldown.toml");
    let config = format!(
        "poll_interval_seconds = 5\n{}",
        live_contract("Alpha", ALPHA, &horizon_url)
    );
    fs::write(&path, &config).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args(["--config", path.to_str().unwrap(), "watch", "--dry-run"])
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start txwatch");

    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });

    wait_for(&lines, "TxWatch polling engine started");
    // First matching transaction fires an alert.
    wait_for(&lines, "alert");

    // Reload with an unchanged config: cooldown state must survive.
    sighup(&child);
    wait_for(&lines, "configuration reloaded");

    // Within the cooldown window no duplicate alert should be emitted.
    let deadline = Instant::now() + Duration::from_secs(3);
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match lines.recv_timeout(remaining) {
            Ok(line) if line.contains("alert") => {
                panic!("duplicate alert after reload despite cooldown: {}", line)
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn watch_reload_migrates_cursor_file_path() {
    let old_cursor = env::temp_dir().join("txwatch_cursor_old.json");
    let new_cursor = env::temp_dir().join("txwatch_cursor_new.json");
    let _ = fs::remove_file(&old_cursor);
    let _ = fs::remove_file(&new_cursor);

    let path = env::temp_dir().join("txwatch_sighup_cursor.toml");
    fs::write(
        &path,
        format!(
            "poll_interval_seconds = 10\ncursor_file = \"{}\"\n{}",
            old_cursor.display(),
            contract("Alpha", ALPHA)
        ),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args(["--config", path.to_str().unwrap(), "watch", "--dry-run"])
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start txwatch");

    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });

    wait_for(&lines, "TxWatch polling engine started");

    // Rewrite the config with an extra contract and reload via SIGHUP.
    fs::write(
        &path,
        format!(
            "poll_interval_seconds = 10\n{}{}",
            contract("Alpha", ALPHA),
            contract("Beta", BETA)
    // Change the cursor path and reload.
    fs::write(
        &path,
        format!(
            "poll_interval_seconds = 10\ncursor_file = \"{}\"\n{}",
            new_cursor.display(),
            contract("Alpha", ALPHA)
        ),
    )
    .unwrap();
    sighup(&child);
    let reloaded = wait_for(&lines, "configuration reloaded");
    assert!(reloaded.contains("contracts=2"), "{}", reloaded);

    // The override must survive the reload: the reloaded contract must still
    // point at the sentinel URL rather than the public Horizon.
    let effective = wait_for(&lines, override_url);
    assert!(
        !effective.contains("horizon.stellar.org"),
        "override dropped on reload: {}",
        effective
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "watcher must still be running"
    );

    let _ = child.kill();
    let _ = child.wait();
    wait_for(&lines, "configuration reloaded");

    // Give the watcher a moment to flush cursors on the next generation.
    thread::sleep(Duration::from_secs(1));

    let _ = child.kill();
    let _ = child.wait();

    // Cursors must only be written to the new path after the reload.
    assert!(
        new_cursor.exists(),
        "cursors should be written to the new cursor_file path"
    );
    let old_len = fs::metadata(&old_cursor).map(|m| m.len()).unwrap_or(0);
    let new_len = fs::metadata(&new_cursor).map(|m| m.len()).unwrap_or(0);
    assert!(
        new_len >= old_len,
        "new cursor file should hold the migrated cursors"
    );
}
