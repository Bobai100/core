use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

const CONTRACT_ID: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";

fn txwatch() -> Command {
    Command::new(env!("CARGO_BIN_EXE_txwatch"))
}

fn fresh_output(name: &str) -> PathBuf {
    let path = env::temp_dir().join(format!("txwatch_init_{name}.toml"));
    let _ = fs::remove_file(&path);
    path
}

fn init(output: &Path, extra: &[&str]) -> Output {
    txwatch()
        .args(["init", "--output", output.to_str().unwrap()])
        .args(extra)
        .output()
        .expect("failed to run txwatch")
}

#[test]
fn init_writes_a_config_that_validates() {
    let output = fresh_output("valid");
    let result = init(
        &output,
        &[
            "--contract-id",
            CONTRACT_ID,
            "--network",
            "mainnet",
            "--webhook-url",
            "https://hooks.example.com/alerts",
        ],
    );
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    let written = fs::read_to_string(&output).unwrap();
    assert!(written.contains(CONTRACT_ID));
    assert!(written.contains(r#"network     = "mainnet""#));

    let validate = txwatch()
        .args(["--config", output.to_str().unwrap(), "validate"])
        .status()
        .unwrap();
    assert!(
        validate.success(),
        "the generated config must pass validate"
    );
}

#[test]
fn init_refuses_to_overwrite_without_force() {
    let output = fresh_output("exists");
    fs::write(&output, "keep me").unwrap();
    let args = [
        "--contract-id",
        CONTRACT_ID,
        "--network",
        "testnet",
        "--webhook-url",
        "https://hooks.example.com/alerts",
    ];

    let result = init(&output, &args);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("--force"));
    assert_eq!(fs::read_to_string(&output).unwrap(), "keep me");

    let mut forced = args.to_vec();
    forced.push("--force");
    assert!(init(&output, &forced).status.success());
    assert!(fs::read_to_string(&output).unwrap().contains(CONTRACT_ID));
}

#[test]
fn init_rejects_invalid_input_without_writing() {
    let output = fresh_output("invalid");
    let result = init(
        &output,
        &[
            "--contract-id",
            "not-a-contract",
            "--network",
            "testnet",
            "--webhook-url",
            "https://hooks.example.com/alerts",
        ],
    );
    assert_eq!(result.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("not a valid Stellar contract address")
    );
    assert!(!output.exists(), "nothing is written when validation fails");
}

/// #228: When stdin is not a terminal (piped) and required flags are missing,
/// the command must fail fast with a clear list of missing flags instead of
/// blocking on stdin or printing a confusing EOF error.
#[test]
fn init_fails_fast_on_non_tty_stdin() {
    let output = fresh_output("non_tty");
    // Spawn with piped stdin (not a TTY) and no required flags.
    let result = txwatch()
        .args(["init", "--output", output.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run txwatch");

    assert_eq!(
        result.status.code(),
        Some(1),
        "expected exit code 1 when stdin is not a TTY and flags are missing"
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("--contract-id"),
        "expected --contract-id in error: {stderr}"
    );
    assert!(
        stderr.contains("--webhook-url"),
        "expected --webhook-url in error: {stderr}"
    );
    assert!(!output.exists(), "no file should be written on fast-fail");
}

/// #228: --non-interactive forces the fail-fast behaviour even when stdin is a TTY.
#[test]
fn init_non_interactive_flag_fails_fast_with_missing_flags() {
    let output = fresh_output("non_interactive");
    let result = init(
        &output,
        &[
            "--non-interactive",
            "--contract-id",
            CONTRACT_ID,
            // webhook-url is intentionally omitted
        ],
    );
    assert_eq!(result.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("--webhook-url"),
        "expected --webhook-url in error: {stderr}"
    );
    assert!(!output.exists());
}

/// #229: --label sets a custom contract label in the generated config.
#[test]
fn init_custom_label() {
    let output = fresh_output("label");
    let result = init(
        &output,
        &[
            "--contract-id",
            CONTRACT_ID,
            "--network",
            "testnet",
            "--webhook-url",
            "https://hooks.example.com/alerts",
            "--label",
            "My Escrow",
        ],
    );
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let written = fs::read_to_string(&output).unwrap();
    assert!(
        written.contains(r#"label       = "My Escrow""#),
        "expected custom label in output:\n{written}"
    );
}

/// #229: --rule presets generate the correct TOML rule entries.
#[test]
fn init_rule_presets() {
    let output = fresh_output("rules");
    let result = init(
        &output,
        &[
            "--contract-id",
            CONTRACT_ID,
            "--network",
            "testnet",
            "--webhook-url",
            "https://hooks.example.com/alerts",
            "--rule",
            "failed",
            "--rule",
            "large-transfer=5000",
            "--rule",
            "admin",
        ],
    );
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let written = fs::read_to_string(&output).unwrap();
    assert!(
        written.contains("TransactionFailed"),
        "expected TransactionFailed rule:\n{written}"
    );
    assert!(
        written.contains("LargeTransfer"),
        "expected LargeTransfer rule:\n{written}"
    );
    assert!(
        written.contains("threshold_xlm = 5000"),
        "expected threshold_xlm:\n{written}"
    );
    assert!(
        written.contains("AdminFunctionCalled"),
        "expected AdminFunctionCalled rule:\n{written}"
    );

    // The generated config must be valid.
    let validate = txwatch()
        .args(["--config", output.to_str().unwrap(), "validate"])
        .status()
        .unwrap();
    assert!(validate.success(), "generated config with rule presets must pass validate");
}

/// #229: --horizon-url embeds a custom horizon_url in the generated config.
#[test]
fn init_custom_horizon_url() {
    let output = fresh_output("horizon_url");
    let result = init(
        &output,
        &[
            "--contract-id",
            CONTRACT_ID,
            "--network",
            "testnet",
            "--webhook-url",
            "https://hooks.example.com/alerts",
            "--horizon-url",
            "http://localhost:8000",
        ],
    );
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let written = fs::read_to_string(&output).unwrap();
    assert!(
        written.contains("horizon_url"),
        "expected horizon_url in output:\n{written}"
    );
    assert!(
        written.contains("http://localhost:8000"),
        "expected custom horizon URL:\n{written}"
    );
}

/// #229: --network validates known network names before prompting.
#[test]
fn init_rejects_unknown_network() {
    let output = fresh_output("bad_network");
    let result = init(
        &output,
        &[
            "--contract-id",
            CONTRACT_ID,
            "--network",
            "invalidnet",
            "--webhook-url",
            "https://hooks.example.com/alerts",
            "--non-interactive",
        ],
    );
    assert_eq!(result.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("unknown network"),
        "expected 'unknown network' error: {stderr}"
    );
    assert!(!output.exists());
}

/// #227: init uses create_new so a file created between the existence check
/// and the write is not silently overwritten (TOCTOU race condition).
/// We verify this indirectly by checking that --force is required to overwrite.
#[test]
fn init_create_new_prevents_silent_overwrite() {
    let output = fresh_output("toctou");
    // Write a file after the path is chosen but simulate it existing already.
    fs::write(&output, "existing content").unwrap();

    let result = init(
        &output,
        &[
            "--contract-id",
            CONTRACT_ID,
            "--network",
            "testnet",
            "--webhook-url",
            "https://hooks.example.com/alerts",
        ],
    );
    // Must fail because the file exists and --force was not given.
    assert_eq!(result.status.code(), Some(1));
    // The original content must be preserved.
    assert_eq!(
        fs::read_to_string(&output).unwrap(),
        "existing content",
        "existing file must not be overwritten without --force"
    );
}
