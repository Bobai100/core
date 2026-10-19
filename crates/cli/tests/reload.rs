//! SIGHUP config reload for `txwatch watch`. Closes #98.
#![cfg(unix)]

use std::{
    env, fs,
    io::{BufRead, BufReader},
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

#[test]
fn watch_reloads_config_on_sighup_and_keeps_old_config_when_invalid() {
    let path = env::temp_dir().join("txwatch_sighup_reload.toml");
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
}

#[test]
fn watch_config_reloads_on_file_change_debounced() {
    let dir = env::temp_dir().join(format!("txwatch_watch_config_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("txwatch.toml");
    fs::write(
        &path,
        format!("poll_interval_seconds = 10\n{}", contract("Alpha", ALPHA)),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args([
            "--config",
            path.to_str().unwrap(),
            "watch",
            "--watch-config",
            "--dry-run",
        ])
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

    // Modify file contents
    fs::write(
        &path,
        format!(
            "poll_interval_seconds = 10\n{}{}",
            contract("Alpha", ALPHA),
            contract("Beta", BETA)
        ),
    )
    .unwrap();

    let change = wait_for(&lines, "config file change detected");
    assert!(change.contains("reloading config"));

    let reloaded = wait_for(&lines, "configuration reloaded");
    assert!(reloaded.contains("contracts=2"), "{}", reloaded);

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn watch_config_reloads_on_symlink_swapped_configmap() {
    let dir = env::temp_dir().join(format!("txwatch_configmap_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();

    let data_v1 = dir.join("data_v1");
    fs::create_dir_all(&data_v1).unwrap();
    fs::write(
        data_v1.join("txwatch.toml"),
        format!("poll_interval_seconds = 10\n{}", contract("Alpha", ALPHA)),
    )
    .unwrap();

    let data_symlink = dir.join("..data");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&data_v1, &data_symlink).unwrap();

    let config_symlink = dir.join("txwatch.toml");
    #[cfg(unix)]
    std::os::unix::fs::symlink(data_symlink.join("txwatch.toml"), &config_symlink).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args([
            "--config",
            config_symlink.to_str().unwrap(),
            "watch",
            "--watch-config",
            "--dry-run",
        ])
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

    // Prepare v2 directory with new contract
    let data_v2 = dir.join("data_v2");
    fs::create_dir_all(&data_v2).unwrap();
    fs::write(
        data_v2.join("txwatch.toml"),
        format!(
            "poll_interval_seconds = 10\n{}{}",
            contract("Alpha", ALPHA),
            contract("Beta", BETA)
        ),
    )
    .unwrap();

    // Atomic symlink swap (just like Kubernetes ConfigMap updates)
    let tmp_symlink = dir.join("..data_tmp");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&data_v2, &tmp_symlink).unwrap();
        fs::rename(&tmp_symlink, &data_symlink).unwrap();
    }

    let change = wait_for(&lines, "config file change detected");
    assert!(change.contains("reloading config"));

    let reloaded = wait_for(&lines, "configuration reloaded");
    assert!(reloaded.contains("contracts=2"), "{}", reloaded);

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn file_watcher_reloads_included_files() {
    let dir = env::temp_dir().join(format!("txwatch_include_reload_{}", std::process::id()));
    let contracts_d = dir.join("contracts.d");
    fs::create_dir_all(&contracts_d).unwrap();

    let root_path = dir.join("txwatch.toml");
    fs::write(
        &root_path,
        format!(
            "poll_interval_seconds = 10\ninclude = [\"contracts.d/*.toml\"]\n{}",
            contract("Alpha", ALPHA)
        ),
    )
    .unwrap();

    let inc_file = contracts_d.join("beta.toml");
    fs::write(&inc_file, contract("Beta", BETA)).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args([
            "--config",
            root_path.to_str().unwrap(),
            "watch",
            "--watch-config",
            "--dry-run",
        ])
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

    // Modify the included file
    fs::write(
        &inc_file,
        format!(
            "{}\n{}",
            contract("Beta", BETA),
            contract(
                "Gamma",
                "CABAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAFNSZ"
            )
        ),
    )
    .unwrap();

    let change = wait_for(&lines, "config file change detected");
    assert!(change.contains("reloading config"));

    let reloaded = wait_for(&lines, "configuration reloaded");
    assert!(reloaded.contains("contracts=3"), "{}", reloaded);

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&dir);
}
