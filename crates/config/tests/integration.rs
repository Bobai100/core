use std::path::PathBuf;
use txwatch_config::AppConfig;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate parent missing")
        .parent()
        .expect("workspace root missing")
        .to_path_buf()
}

fn crate_fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn example_toml_parses_and_validates_successfully() {
    let path = project_root().join("config").join("example.toml");
    let cfg =
        AppConfig::from_file(&path).expect("example.toml should parse and validate successfully");

    assert_eq!(cfg.poll_interval_seconds, 10);
    assert!(
        !cfg.contracts.is_empty(),
        "example config should define contracts"
    );
}

#[test]
fn local_toml_parses_and_validates_successfully() {
    let path = project_root().join("config").join("local.toml");
    let cfg =
        AppConfig::from_file(&path).expect("local.toml should parse and validate successfully");

    assert_eq!(cfg.contracts[0].network.as_str(), "custom");
    assert_eq!(
        cfg.contracts[0].network.horizon_base_url(),
        "http://stellar:8000"
    );
}

#[test]
fn broken_toml_fixture_fails_with_meaningful_error() {
    let path = crate_fixture_path("broken-example.toml");
    let err = AppConfig::from_file(&path).expect_err("broken TOML fixture should fail");
    let msg = err.to_string();

    assert!(
        msg.contains("threshold_xlm must be > 0"),
        "expected a validation failure, got: {}",
        msg
    );
}

#[test]
fn multi_file_include_with_deterministic_ordering() {
    let dir = std::env::temp_dir().join(format!("txwatch-test-multi-{}", std::process::id()));
    let contracts_d = dir.join("contracts.d");
    std::fs::create_dir_all(&contracts_d).unwrap();

    let root_toml = dir.join("txwatch.toml");
    std::fs::write(
        &root_toml,
        r#"
poll_interval_seconds = 10
include = ["contracts.d/*.toml"]
"#,
    )
    .unwrap();

    let contract_b = contracts_d.join("02_token.toml");
    std::fs::write(
        &contract_b,
        r#"
[[contracts]]
label = "Token"
contract_id = "CAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC526"
network = "testnet"
webhook_url = "https://example.com/b"
[[contracts.rules]]
type = "AnyTransaction"
"#,
    )
    .unwrap();

    let contract_a = contracts_d.join("01_vault.toml");
    std::fs::write(
        &contract_a,
        r#"
[[contracts]]
label = "Vault"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
network = "testnet"
webhook_url = "https://example.com/a"
[[contracts.rules]]
type = "AnyTransaction"
"#,
    )
    .unwrap();

    let cfg =
        AppConfig::from_file(&root_toml).expect("multi-file config should parse and validate");
    assert_eq!(cfg.contracts.len(), 2);
    // Deterministic order: 01_vault.toml before 02_token.toml
    assert_eq!(cfg.contracts[0].label, "Vault");
    assert_eq!(cfg.contracts[1].label, "Token");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn multi_file_reports_errors_with_originating_file_name() {
    let dir = std::env::temp_dir().join(format!("txwatch-test-err-{}", std::process::id()));
    let contracts_d = dir.join("contracts.d");
    std::fs::create_dir_all(&contracts_d).unwrap();

    let root_toml = dir.join("txwatch.toml");
    std::fs::write(
        &root_toml,
        r#"
poll_interval_seconds = 10
include = ["contracts.d/*.toml"]
"#,
    )
    .unwrap();

    let broken_file = contracts_d.join("invalid_contract.toml");
    std::fs::write(
        &broken_file,
        r#"
[[contracts]]
label = "Bad"
contract_id = "CSHORT"
network = "testnet"
webhook_url = "https://example.com/hook"
[[contracts.rules]]
type = "AnyTransaction"
"#,
    )
    .unwrap();

    let err = AppConfig::from_file(&root_toml).expect_err("should fail on invalid contract");
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("invalid_contract.toml"),
        "error must report originating file name, got: {}",
        err_msg
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn circular_include_is_detected_and_rejected() {
    let dir = std::env::temp_dir().join(format!("txwatch-test-circular-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let file_a = dir.join("a.toml");
    let file_b = dir.join("b.toml");

    std::fs::write(
        &file_a,
        r#"
poll_interval_seconds = 10
include = ["b.toml"]
"#,
    )
    .unwrap();

    std::fs::write(
        &file_b,
        r#"
include = ["a.toml"]
"#,
    )
    .unwrap();

    let err = AppConfig::from_file(&file_a).expect_err("circular include should fail");
    assert!(err.to_string().contains("circular include detected"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compute_config_hash_detects_changes_in_included_files() {
    let dir = std::env::temp_dir().join(format!("txwatch-test-hash-{}", std::process::id()));
    let contracts_d = dir.join("contracts.d");
    std::fs::create_dir_all(&contracts_d).unwrap();

    let root_toml = dir.join("txwatch.toml");
    std::fs::write(
        &root_toml,
        r#"
poll_interval_seconds = 10
include = ["contracts.d/*.toml"]
"#,
    )
    .unwrap();

    let inc_toml = contracts_d.join("token.toml");
    std::fs::write(&inc_toml, "# version 1\n").unwrap();

    let hash1 = txwatch_config::compute_config_hash(&root_toml).expect("hash1");

    // Modify the included file
    std::fs::write(&inc_toml, "# version 2\n").unwrap();
    let hash2 = txwatch_config::compute_config_hash(&root_toml).expect("hash2");

    assert_ne!(hash1, hash2, "hash must change when included file changes");

    let _ = std::fs::remove_dir_all(&dir);
}
