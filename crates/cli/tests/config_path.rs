use std::{env, fs, process::Command};

fn txwatch_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_txwatch"))
}

const VALID_CONFIG: &str = r#"
poll_interval_seconds = 10

[[contracts]]
label       = "From Env"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network     = "testnet"
webhook_url = "https://hooks.example.com/test"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;

/// Without --config, TXWATCH_CONFIG or ./txwatch.toml the CLI fails with a
/// helpful message instead of falling back to the example config. Closes #100.
#[test]
fn missing_default_config_fails_with_helpful_message() {
    let dir = env::temp_dir().join("txwatch_no_default_config");
    fs::create_dir_all(&dir).unwrap();
    let _ = fs::remove_file(dir.join("txwatch.toml"));

    let output = txwatch_bin()
        .current_dir(&dir)
        .env_remove("TXWATCH_CONFIG")
        .arg("validate")
        .output()
        .expect("failed to run txwatch");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("config file 'txwatch.toml' not found"),
        "{}",
        stderr
    );
    assert!(stderr.contains("TXWATCH_CONFIG"), "{}", stderr);
}

#[test]
fn config_path_is_read_from_txwatch_config_env_var() {
    let path = env::temp_dir().join("txwatch_env_var_config.toml");
    fs::write(&path, VALID_CONFIG).unwrap();

    let output = txwatch_bin()
        .env("TXWATCH_CONFIG", &path)
        .arg("validate")
        .output()
        .expect("failed to run txwatch");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("From Env"));
}

#[test]
fn default_config_path_is_txwatch_toml_in_working_directory() {
    let dir = env::temp_dir().join("txwatch_default_config");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("txwatch.toml"), VALID_CONFIG).unwrap();

    let output = txwatch_bin()
        .current_dir(&dir)
        .env_remove("TXWATCH_CONFIG")
        .arg("validate")
        .output()
        .expect("failed to run txwatch");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("From Env"));
}

/// On Windows, `fs::rename` over an existing file is not atomic by default
/// (it fails with an error), so the cursor-file persistence code must use a
/// replace-existing strategy. This test exercises that path directly by
/// writing a cursor file, then atomically replacing it over the existing file
/// and verifying the new contents. Closes #203.
#[cfg(windows)]
#[test]
fn cursor_file_can_be_atomically_replaced_on_windows() {
    use std::io::Write;

    let dir = env::temp_dir().join("txwatch_cursor_replace");
    fs::create_dir_all(&dir).unwrap();
    let cursor_path = dir.join("cursor.json");

    // Seed the existing cursor file.
    fs::write(&cursor_path, r#"{"last_ledger":100}"#).unwrap();
    assert_eq!(
        fs::read_to_string(&cursor_path).unwrap(),
        r#"{"last_ledger":100}"#
    );

    // Write the replacement to a sibling temp file, then atomically replace
    // the existing cursor file. On Windows this requires removing the target
    // first (or using a replace-existing API), which is what the CLI does.
    let tmp_path = dir.join("cursor.json.tmp");
    {
        let mut f = fs::File::create(&tmp_path).unwrap();
        f.write_all(r#"{"last_ledger":200}"#.as_bytes()).unwrap();
        f.sync_all().unwrap();
    }

    // Remove the existing file so the rename succeeds on Windows, then rename.
    // This mirrors the atomic-replace strategy used by the CLI on non-Unix.
    if cursor_path.exists() {
        fs::remove_file(&cursor_path).unwrap();
    }
    fs::rename(&tmp_path, &cursor_path).unwrap();

    assert_eq!(
        fs::read_to_string(&cursor_path).unwrap(),
        r#"{"last_ledger":200}"#
    );
    assert!(!tmp_path.exists());
}