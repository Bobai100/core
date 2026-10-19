//! `txwatch watch --metrics-addr` serves Prometheus metrics (feature `metrics`).
#![cfg(feature = "metrics")]

use std::{
    env, fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const CONFIG: &str = r#"
poll_interval_seconds = 3600

[[contracts]]
label       = "Metrics Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network     = "testnet"
webhook_url = "https://hooks.example.com/metrics"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;

/// Kills the child process when the test ends, even on panic.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn http_get(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    Some(response)
}

#[test]
fn watch_serves_prometheus_metrics() {
    let config = env::temp_dir().join("txwatch_metrics_cli_test.toml");
    fs::write(&config, CONFIG).unwrap();
    let port = free_port();

    let _child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_txwatch"))
            .args([
                "--config",
                config.to_str().unwrap(),
                "watch",
                "--dry-run",
                "--metrics-addr",
                &format!("127.0.0.1:{port}"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start txwatch"),
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    let body = loop {
        if let Some(response) = http_get(port, "/metrics") {
            if response.contains("txwatch_build_info") {
                break response;
            }
        }
        assert!(
            Instant::now() < deadline,
            "/metrics never served txwatch_build_info"
        );
        thread::sleep(Duration::from_millis(200));
    };

    assert!(
        body.starts_with("HTTP/1.1 200"),
        "unexpected response: {body}"
    );
    assert!(body.contains("txwatch_build_info"));

    // The version label must match the crate version.
    let build_info_line = body
        .lines()
        .find(|line| line.starts_with("txwatch_build_info"))
        .expect("txwatch_build_info line missing from /metrics");
    assert!(
        build_info_line.contains(&format!("version=\"{}\"", env!("CARGO_PKG_VERSION"))),
        "txwatch_build_info version label does not match crate version: {build_info_line}"
    );

    // Guard against issue #206: in a git checkout the git_sha label must be
    // populated, not the compile-time fallback `unknown`.
    let in_git_checkout = std::path::Path::new(".git").exists()
        || Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    if in_git_checkout {
        assert!(
            !build_info_line.contains("git_sha=\"unknown\""),
            "txwatch_build_info git_sha is `unknown` in a git checkout: {build_info_line}"
        );
    }
}
