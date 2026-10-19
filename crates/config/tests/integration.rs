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
fn invalid_syntax_toml_fixture_reports_file_path() {
    let path = crate_fixture_path("invalid-syntax.toml");
    let err = AppConfig::from_file(&path).expect_err("invalid TOML fixture should fail");
    let msg = err.to_string();

    assert!(
        msg.contains("invalid-syntax.toml"),
        "expected the error to include the file path, got: {}",
        msg
    );
fn write_temp_config(name: &str, contents: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "txwatch-config-it-{}-{}",
        std::process::id(),
        name
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("config.toml");
    std::fs::write(&path, contents).expect("write temp config");
    path
}

const CONTRACT_HEADER: &str = r#"
poll_interval_seconds = 10

[[contracts]]
name = "token"
network = "testnet"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
"#;

#[test]
fn event_emitted_data_matchers_parse_and_validate() {
    let toml = format!(
        r#"{header}
[[contracts.rules]]
type = "event_emitted"
topic = ["transfer"]
data = [
  {{ pointer = "/amount", gte = "1000" }},
  {{ pointer = "/to", eq = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF" }},
]
"#,
        header = CONTRACT_HEADER
    );
    let path = write_temp_config("data-matchers-ok", &toml);
    AppConfig::from_file(&path).expect("data matchers should parse and validate");
}

#[test]
fn event_emitted_typed_topic_patterns_parse() {
    let toml = format!(
        r#"{header}
[[contracts.rules]]
type = "event_emitted"
topic = ["address:GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF", "u32:5"]
"#,
        header = CONTRACT_HEADER
    );
    let path = write_temp_config("typed-topics-ok", &toml);
    AppConfig::from_file(&path).expect("typed topic patterns should parse");
}

#[test]
fn event_emitted_rejects_invalid_json_pointer() {
    let toml = format!(
        r#"{header}
[[contracts.rules]]
type = "event_emitted"
topic = ["transfer"]
data = [{{ pointer = "amount", eq = "1" }}]
"#,
        header = CONTRACT_HEADER
    );
    let path = write_temp_config("bad-pointer", &toml);
    let err = AppConfig::from_file(&path).expect_err("invalid pointer should be rejected");
    assert!(
        err.to_string().to_lowercase().contains("pointer"),
        "expected pointer validation error, got: {}",
        err
    );
}

#[test]
fn event_emitted_rejects_incompatible_comparison_type() {
    let toml = format!(
        r#"{header}
[[contracts.rules]]
type = "event_emitted"
topic = ["transfer"]
data = [{{ pointer = "/amount", gte = "not-a-number" }}]
"#,
        header = CONTRACT_HEADER
    );
    let path = write_temp_config("bad-comparison", &toml);
    let err = AppConfig::from_file(&path).expect_err("bad comparison should be rejected");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("gte") || msg.contains("comparison") || msg.contains("number"),
        "expected comparison type validation error, got: {}",
        err
    );
}

#[test]
fn large_transfer_direction_parses_and_validates() {
    for direction in ["in", "out", "any"] {
        let toml = format!(
            r#"{header}
[[contracts.rules]]
type = "large_transfer"
threshold_xlm = 100.0
direction = "{direction}"
"#,
            header = CONTRACT_HEADER,
            direction = direction
        );
        let path = write_temp_config(&format!("direction-{}", direction), &toml);
        AppConfig::from_file(&path).expect("valid direction should parse and validate");
    }

    let toml = format!(
        r#"{header}
[[contracts.rules]]
type = "large_transfer"
threshold_xlm = 100.0
direction = "sideways"
"#,
        header = CONTRACT_HEADER
    );
    let path = write_temp_config("direction-bad", &toml);
    AppConfig::from_file(&path).expect_err("invalid direction should be rejected");
}

#[test]
fn contract_upgraded_rule_parses() {
    let toml = format!(
        r#"{header}
[[contracts.rules]]
type = "contract_upgraded"
"#,
        header = CONTRACT_HEADER
    );
    let path = write_temp_config("contract-upgraded", &toml);
    AppConfig::from_file(&path).expect("contract_upgraded rule should parse");
}
