use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use futures::future::join_all;
use reqwest::{Client, StatusCode};
use tokio::sync::watch;
use tracing::{error, info, warn};
use txwatch_config::{AppConfig, WebhookDestination, WebhookFormat, WebhookHeaders, REDACTED};
use txwatch_notifier::{build_client, send_to_destination_simple, test_payload_with_network};

// ── CLI definition ────────────────────────────────────────────────────────────

const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("TXWATCH_GIT_SHA"),
    " built ",
    env!("TXWATCH_BUILD_TIMESTAMP"),
    ")"
);

#[derive(Parser)]
#[command(
    name    = "txwatch",
    version = VERSION,
    about   = "Stellar Soroban contract monitor & webhook alert engine"
)]
struct Cli {
    /// Path to the TOML config file
    #[arg(short, long, env = "TXWATCH_CONFIG", default_value = "txwatch.toml")]
    config: PathBuf,

    /// Log output format: human-readable text or one JSON object per line
    #[arg(
        long,
        global = true,
        value_enum,
        env = "TXWATCH_LOG_FORMAT",
        default_value = "text"
    )]
    log_format: LogFormat,

    /// Override the Horizon base URL for every contract (e.g. a private Horizon instance)
    #[arg(long, global = true)]
    horizon_url: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Subcommand)]
enum Command {
    /// Start the polling engine (watches all contracts in the config)
    ///
    /// With --once, exit codes: 0 = every poll and webhook delivery succeeded, 1 = otherwise
    Watch {
        /// Do not actually send webhooks; only log matched rules
        #[arg(long)]
        dry_run: bool,

        /// Serve Prometheus /metrics (plus /healthz and /readyz) on this address, e.g. 127.0.0.1:9090
        #[cfg(feature = "metrics")]
        #[arg(long)]
        metrics_addr: Option<std::net::SocketAddr>,
        /// Run a single poll cycle, deliver alerts, save cursors and exit
        #[arg(long)]
        once: bool,

        /// Reload configuration when the config file or included files change
        #[arg(long, env = "TXWATCH_WATCH_CONFIG")]
        watch_config: bool,
    },

    /// Parse and validate the config file, then print a summary
    ///
    /// Exit codes: 0 = valid config, 1 = invalid or missing config
    Validate {
        /// Send a HEAD/OPTIONS request to each webhook URL and warn on unreachable endpoints.
        #[arg(long)]
        check_webhooks: bool,

        /// Verify that each contract exists on its configured Horizon network.
        #[arg(long)]
        check_horizon: bool,

        /// Output format. `json` prints the parsed config (secrets redacted) or the
        /// validation error as a single JSON object on stdout.
        #[arg(long, value_enum, default_value = "text", conflicts_with_all = ["check_webhooks", "check_horizon"])]
        format: OutputFormat,
    },

    /// Send a test webhook payload to a URL and exit
    ///
    /// Exit codes: 0 = webhook delivered, 1 = delivery failed (unreachable or HTTP error)
    TestWebhook {
        /// The webhook URL to POST to
        #[arg(long)]
        url: Option<String>,

        /// Label to include in the test payload
        #[arg(long, default_value = "TxWatch Test")]
        label: String,

        #[arg(long, default_value = "testnet")]
        network: String,

        /// Send to every destination of this configured contract instead of --url
        #[arg(long)]
        contract: Option<String>,

        /// Webhook secret (overrides the configured ones with --contract)
        #[arg(long)]
        secret: Option<String>,

        /// Body format for --url: txwatch, slack, discord or pagerduty
        #[arg(long, value_parser = parse_webhook_format, default_value = "txwatch")]
        format: WebhookFormat,

        /// PagerDuty routing key for --url with --format pagerduty
        #[arg(long)]
        routing_key: Option<String>,

        /// Extra header for --url as "Name: value"; repeatable
        #[arg(long = "header", value_name = "NAME: VALUE")]
        headers: Vec<String>,
    },

    /// Print the JSON Schema for the TOML configuration file.
    Schema,

    /// Print a shell completion script, e.g. `txwatch completions bash > /etc/bash_completion.d/txwatch`
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },

    /// Print the txwatch(1) man page in roff format, e.g. `txwatch man > txwatch.1`
    Man,

    /// Write a starter config file for one contract
    ///
    /// Missing values are prompted for on stdin. The result is validated before
    /// it is written, and an existing file is never overwritten without --force.
    Init {
        /// Soroban contract address to watch (56 characters, starts with 'C')
        #[arg(long)]
        contract_id: Option<String>,

        /// Stellar network: mainnet, testnet or futurenet
        #[arg(long)]
        network: Option<String>,

        /// URL that receives webhook alerts
        #[arg(long)]
        webhook_url: Option<String>,

        /// Where to write the config
        #[arg(long, default_value = "txwatch.toml")]
        output: PathBuf,

        /// Overwrite the output file if it already exists
        #[arg(long)]
        force: bool,
    },

    /// Evaluate a contract's rules against one historical transaction
    ///
    /// Prints the rules that matched and their webhook payloads. Nothing is sent
    /// unless --send is given. Exit codes: 0 = done, 1 = lookup or delivery failed
    Replay {
        /// Label of the configured contract whose rules to evaluate
        #[arg(long)]
        contract: String,

        /// Transaction hash to replay
        #[arg(long)]
        tx: String,

        /// Also deliver the resulting webhooks to the contract's webhook_url
        #[arg(long)]
        send: bool,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.log_format);

    match cli.command {
        Command::Validate {
            format: OutputFormat::Json,
            ..
        } => match AppConfig::from_file(&required_config(&cli.config)?) {
            Ok(cfg) => println!(
                "{}",
                serde_json::to_string_pretty(&config_summary_json(&cfg))?
            ),
            Err(e) => {
                let error = serde_json::json!({ "valid": false, "error": format!("{:#}", e) });
                println!("{}", serde_json::to_string_pretty(&error)?);
                std::process::exit(1);
            }
        },

        Command::Validate {
            check_webhooks,
            check_horizon,
            ..
        } => {
            let cfg = load_config(&cli)?;
            println!("Config is valid.");
            println!("  poll_interval_seconds : {}", cfg.poll_interval_seconds);
            println!("  contracts             : {}", cfg.contracts.len());
            println!();
            for c in &cfg.contracts {
                println!(
                    "  [{network}] {label}{disabled}",
                    network = c.network.display_name(),
                    label = c.label,
                    disabled = if c.enabled { "" } else { " (disabled)" }
                );
                println!("    contract_id  : {}", c.contract_id);
                let destinations = c.destinations();
                println!("    webhooks     : {}", destinations.len());
                for destination in &destinations {
                    println!("      - {}", describe_destination(destination));
                }
                println!(
                    "    interval     : {}s{}",
                    c.effective_poll_interval(cfg.poll_interval_seconds),
                    if c.poll_interval_seconds.is_some() {
                        " (override)"
                    } else {
                        ""
                    }
                );
                println!("    rules        : {}", c.rules.len());
                for entry in &c.rules {
                    println!("      - {}", entry.rule.label());
                }
                println!("    horizon      : {}", c.network.horizon_base_url());
                match c.network.explorer_base_url() {
                    Some(explorer) => {
                        println!("    explorer     : {}/contract/{}", explorer, c.contract_id)
                    }
                    None => println!("    explorer     : none"),
                }
            }

            if check_webhooks {
                let client = Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .context("failed to build HTTP client")?;
                let targets: Vec<(String, String)> = cfg
                    .contracts
                    .iter()
                    .flat_map(|c| {
                        c.destinations()
                            .into_iter()
                            .map(move |d| (c.label.clone(), d.url))
                    })
                    .collect();
                let checks = join_all(targets.into_iter().map(|(label, url)| {
                    let client = &client;
                    async move {
                        let result = check_webhook_reachable(client, &url).await;
                        (label, url, result)
                    }
                }))
                .await;
                let mut failed = false;
                println!("Webhook checks:");
                for (label, url, result) in checks {
                    match result {
                        Ok(status) => {
                            if status == "method not allowed" {
                                failed = true;
                            }
                            println!("  {:<18} {:<20} {}", label, status, url)
                        }
                        Err(error) => {
                            failed = true;
                            println!("  {:<18} {:<20} {} ({})", label, "unreachable", url, error);
                        }
                    }
                }
                if failed {
                    return Err(anyhow::anyhow!("one or more webhook checks failed"));
                }
            }
            if check_horizon {
                let client = Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .context("failed to build HTTP client")?;
                let checks = join_all(
                    cfg.contracts
                        .iter()
                        .map(|c| check_horizon_contract(&client, c)),
                )
                .await;
                let mut failed = false;
                println!("Horizon checks:");
                for check in checks {
                    println!(
                        "  {:<10} {:<12} latest ledger: {} — {}",
                        check.network,
                        check.status,
                        check
                            .latest_ledger
                            .map_or_else(|| "unknown".into(), |n| n.to_string()),
                        check.message
                    );
                    failed |= !check.reachable || !check.found;
                }
                if failed {
                    return Err(anyhow::anyhow!("one or more Horizon checks failed"));
                }
            }
        }

        Command::TestWebhook {
            url,
            label,
            network,
            contract,
            secret,
            format,
            routing_key,
            headers,
        } => {
            let configured = contract
                .as_ref()
                .map(|wanted| {
                    let cfg = AppConfig::from_file(&required_config(&cli.config)?)?;
                    cfg.contracts
                        .into_iter()
                        .find(|c| c.label == *wanted)
                        .ok_or_else(|| {
                            anyhow::anyhow!("configured contract '{}' not found", wanted)
                        })
                })
                .transpose()?;
            let (destinations, network) = if let Some(c) = configured {
                let mut destinations = c.destinations();
                if secret.is_some() {
                    for destination in &mut destinations {
                        destination.secret = secret.clone();
                    }
                }
                (destinations, c.network)
            } else {
                let selected = match network.as_str() {
                    "mainnet" => txwatch_config::Network::Mainnet,
                    "testnet" => txwatch_config::Network::Testnet,
                    "futurenet" => txwatch_config::Network::Futurenet,
                    other => return Err(anyhow::anyhow!("unknown network '{}'", other)),
                };
                let destination = WebhookDestination {
                    url: url.ok_or_else(|| {
                        anyhow::anyhow!("--url is required unless --contract is provided")
                    })?,
                    secret,
                    format,
                    headers: parse_headers(&headers)?,
                    routing_key,
                    ..Default::default()
                };
                destination.validate()?;
                (vec![destination], selected)
            };
            let client = build_client().context("failed to build HTTP client")?;

            // Try every destination, then fail if any of them failed.
            let mut failed = 0;
            for destination in &destinations {
                let payload = test_payload_with_network(
                    &label,
                    network.as_str(),
                    network.horizon_base_url(),
                    network.explorer_base_url(),
                );
                info!(url = %destination.url, format = %destination.format, "sending test webhook");
                match send_to_destination_simple(&client, destination, &payload).await {
                    Ok(result) => println!(
                        "Test webhook delivered successfully to {} (format {}, status {}, attempts {})",
                        destination.url, destination.format, result.final_status, result.attempts
                    ),
                    Err(e) => {
                        failed += 1;
                        eprintln!("test webhook to '{}' failed: {:#}", destination.url, e);
                    }
                }
            }
            if failed > 0 {
                return Err(anyhow::anyhow!(
                    "{} of {} test webhook(s) failed",
                    failed,
                    destinations.len()
                ));
            }
        }

        Command::Init {
            ref contract_id,
            ref network,
            ref webhook_url,
            ref output,
            force,
        } => {
            if output.exists() && !force {
                return Err(anyhow::anyhow!(
                    "'{}' already exists; pass --force to overwrite it",
                    output.display()
                ));
            }
            let contract_id = value_or_prompt(contract_id, "Contract ID (C...)")?;
            let network = match network {
                Some(n) => n.clone(),
                None => prompt("Network [testnet]")?
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| "testnet".into()),
            };
            let webhook_url = value_or_prompt(webhook_url, "Webhook URL")?;

            let raw = render_init_config(&contract_id, &network, &webhook_url)?;
            // Validate with the config crate before touching the filesystem.
            AppConfig::parse(&raw, output).context("the generated config is not valid")?;
            fs::write(output, raw)
                .with_context(|| format!("failed to write '{}'", output.display()))?;
            println!(
                "Wrote {}. Check it with: txwatch --config {} validate",
                output.display(),
                output.display()
            );
        }

        Command::Completions { shell } => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "txwatch",
                &mut std::io::stdout(),
            );
        }

        Command::Man => {
            match clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout()) {
                // The reader (e.g. `| head`) closing early isn't an error.
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                other => other.context("failed to render the man page")?,
            }
        }

        Command::Replay {
            ref contract,
            ref tx,
            send,
        } => {
            let cfg = load_config(&cli)?;
            let contract = cfg
                .contracts
                .into_iter()
                .find(|c| c.label == *contract)
                .ok_or_else(|| anyhow::anyhow!("configured contract '{}' not found", contract))?;
            let client = build_client().context("failed to build HTTP client")?;

            let payloads = txwatch_poller::replay_transaction(&client, &contract, tx).await?;
            println!(
                "{} rule(s) matched transaction {} for '{}'",
                payloads.len(),
                tx,
                contract.label
            );
            for payload in &payloads {
                println!();
                println!("  rule: {}", payload.rule_triggered);
                println!("{}", serde_json::to_string_pretty(payload)?);
            }

            if send {
                let destinations = contract.destinations();
                let mut failed = 0;
                for payload in &payloads {
                    for destination in &destinations {
                        match send_to_destination_simple(&client, destination, payload).await {
                            Ok(result) => println!(
                                "Delivered '{}' to {} (status {})",
                                payload.rule_triggered, destination.url, result.final_status
                            ),
                            Err(e) => {
                                failed += 1;
                                eprintln!(
                                    "webhook for rule '{}' to '{}' failed: {:#}",
                                    payload.rule_triggered, destination.url, e
                                );
                            }
                        }
                    }
                }
                if failed > 0 {
                    return Err(anyhow::anyhow!("{} webhook delivery(ies) failed", failed));
                }
            }
        }

        Command::Schema => println!(
            "{}",
            serde_json::to_string_pretty(&schemars::schema_for!(txwatch_config::AppConfig))?
        ),

        Command::Watch {
            dry_run,
            once,
            watch_config,
            #[cfg(feature = "metrics")]
            metrics_addr,
        } => {
            let config_path = required_config(&cli.config)?;
            let cfg = load_config(&cli)?;

            if once {
                info!(
                    version = VERSION,
                    contracts = cfg.contracts.len(),
                    dry_run,
                    "running a single TxWatch poll cycle"
                );
                let report = txwatch_poller::run_once(cfg, dry_run).await?;
                info!(
                    transactions = report.transactions,
                    alerts = report.alerts,
                    poll_failures = report.poll_failures,
                    webhook_failures = report.webhook_failures,
                    "poll cycle finished"
                );
                if !report.is_success() {
                    return Err(anyhow::anyhow!(
                        "poll cycle had {} failed contract poll(s) and {} failed webhook delivery(ies)",
                        report.poll_failures,
                        report.webhook_failures
                    ));
                }
                return Ok(());
            }

            // Graceful shutdown: allow the current poll cycle to finish before exiting.
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            tokio::spawn(async move {
                if let Err(e) = tokio::signal::ctrl_c().await {
                    warn!(error = ?e, "failed to install Ctrl+C handler");
                    return;
                }
                let _ = shutdown_tx.send(true);
            });

            #[cfg(feature = "metrics")]
            if let Some(addr) = metrics_addr {
                // Register before serving so the first scrape already has it.
                txwatch_poller::metrics::register_build_info();
                txwatch_poller::serve_metrics(addr, shutdown_rx.clone()).await?;
            }
            let (reload_tx, reload_rx) = tokio::sync::mpsc::channel(1);
            spawn_reload_on_sighup(
                config_path.clone(),
                reload_tx.clone(),
                cli.horizon_url.clone(),
            )?;

            let watch_enabled = watch_config || cfg!(windows);
            if watch_enabled {
                spawn_config_watcher(config_path, reload_tx, cli.horizon_url.clone());
            }

            info!(
                version = VERSION,
                contracts = cfg.contracts.len(),
                interval_secs = cfg.poll_interval_seconds,
                dry_run = dry_run,
                "starting TxWatch"
            );
            txwatch_poller::run_with_reload(cfg, dry_run, shutdown_rx, reload_rx).await?;
        }
    }

    Ok(())
}
/// Read one trimmed line from stdin after printing `label`; `None` on EOF.
fn prompt(label: &str) -> Result<Option<String>> {
    use std::io::{BufRead, Write};
    eprint!("{label}: ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    let read = std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("failed to read from stdin")?;
    Ok((read > 0).then(|| line.trim().to_owned()))
}

fn value_or_prompt(value: &Option<String>, label: &str) -> Result<String> {
    match value {
        Some(v) => Ok(v.clone()),
        None => prompt(label)?
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("{label} is required")),
    }
}

/// Starter config for `txwatch init`: one contract and an AnyTransaction rule.
fn render_init_config(contract_id: &str, network: &str, webhook_url: &str) -> Result<String> {
    // TOML basic strings share JSON's escaping, so serde_json quotes values safely.
    let quote = |s: &str| serde_json::to_string(s);
    Ok(format!(
        r#"# Generated by `txwatch init`. See docs/configuration.md for every option.
poll_interval_seconds = 10

[[contracts]]
label       = "My Contract"
contract_id = {contract_id}
network     = {network}
webhook_url = {webhook_url}
# webhook_secret = "${{TXWATCH_WEBHOOK_SECRET}}"   # optional: signs each webhook

  [[contracts.rules]]
  type = "AnyTransaction"
"#,
        contract_id = quote(contract_id)?,
        network = quote(network)?,
        webhook_url = quote(webhook_url)?,
    ))
}

/// The config path from `--config`, `TXWATCH_CONFIG` or the `./txwatch.toml`
/// default, which must exist.
fn required_config(path: &Path) -> Result<PathBuf> {
    if !path.exists() {
        anyhow::bail!(
            "config file '{}' not found. Pass --config <path>, set TXWATCH_CONFIG, \
             or create ./txwatch.toml (config/example.toml is a starting point)",
            path.display()
        );
    }
    Ok(path.to_path_buf())
}

/// On SIGHUP, re-read and validate the config file and hand it to the poller.
/// An invalid file is logged and ignored, so the previous config keeps running.
#[cfg(unix)]
fn spawn_reload_on_sighup(
    path: PathBuf,
    reload_tx: tokio::sync::mpsc::Sender<AppConfig>,
    horizon_url: Option<String>,
) -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut hangup = signal(SignalKind::hangup()).context("failed to install SIGHUP handler")?;
    tokio::spawn(async move {
        while hangup.recv().await.is_some() {
            info!(path = %path.display(), "SIGHUP received — reloading config");
            if let Some(cfg) = reload_config(&path, horizon_url.as_deref()) {
                if reload_tx.send(cfg).await.is_err() {
                    break;
                }
            }
        }
    });
    Ok(())
}

#[cfg(not(unix))]
fn spawn_reload_on_sighup(
    _path: PathBuf,
    _reload_tx: tokio::sync::mpsc::Sender<AppConfig>,
    _horizon_url: Option<String>,
) -> Result<()> {
    Ok(())
}

/// Watches the configuration file and any included files for changes.
/// Debounces changes to prevent partial reads and correctly detects
/// symlink swaps (common in Kubernetes ConfigMap volume mounts).
fn spawn_config_watcher(
    path: PathBuf,
    reload_tx: tokio::sync::mpsc::Sender<AppConfig>,
    horizon_url: Option<String>,
) {
    tokio::spawn(async move {
        let mut last_hash = txwatch_config::compute_config_hash(&path).ok();
        let poll_interval = Duration::from_millis(250);
        let debounce_interval = Duration::from_millis(250);

        loop {
            tokio::time::sleep(poll_interval).await;

            let current_hash = match txwatch_config::compute_config_hash(&path) {
                Ok(h) => h,
                Err(_) => continue,
            };

            if last_hash.as_ref() == Some(&current_hash) {
                continue;
            }

            // Debounce: wait until hash is stable across debounce_interval
            tokio::time::sleep(debounce_interval).await;
            let debounced_hash = match txwatch_config::compute_config_hash(&path) {
                Ok(h) => h,
                Err(_) => continue,
            };

            if debounced_hash != current_hash {
                continue;
            }

            last_hash = Some(debounced_hash);
            info!(path = %path.display(), "config file change detected — reloading config");

            if let Some(cfg) = reload_config(&path, horizon_url.as_deref()) {
                if reload_tx.send(cfg).await.is_err() {
                    break;
                }
            }
        }
    });
}

fn reload_config(path: &Path, horizon_url: Option<&str>) -> Option<AppConfig> {
    match AppConfig::from_file(path) {
        Ok(mut cfg) => {
            if let Some(url) = horizon_url {
                let url = url.trim_end_matches('/');
                for c in &mut cfg.contracts {
                    c.horizon_base_url_override = Some(url.to_string());
                }
            }
            Some(cfg)
        }
        Err(e) => {
            error!(error = %format!("{:#}", e), "config reload failed — keeping the previous config");
            None
        }
    }
}

/// Load the config and apply `--horizon-url`, if given, to every contract.
fn load_config(cli: &Cli) -> Result<AppConfig> {
    let mut cfg = AppConfig::from_file(&required_config(&cli.config)?)?;
    if let Some(url) = &cli.horizon_url {
        let url = url.trim_end_matches('/');
        for c in &mut cfg.contracts {
            c.horizon_base_url_override = Some(url.to_string());
        }
    }
    Ok(cfg)
}

/// Machine-readable `validate` summary. Webhook secrets are never printed;
/// only whether one is set.
fn config_summary_json(cfg: &AppConfig) -> serde_json::Value {
    let contracts: Vec<_> = cfg
        .contracts
        .iter()
        .map(|c| {
            let rules: Vec<_> = c.rules.iter().map(|entry| {
                let mut obj = serde_json::to_value(&entry.rule)
                    .unwrap_or_else(|_| serde_json::json!({}));
                let map = obj.as_object_mut().unwrap();
                if let Some(cooldown) = entry.cooldown_seconds {
                    map.insert("cooldown_seconds".to_string(), serde_json::json!(cooldown));
                }
                serde_json::Value::Object(map.clone())
            }).collect();
            serde_json::json!({
                "label": c.label,
                "contract_id": c.contract_id,
                "network": c.network.as_str(),
                "enabled": c.enabled,
                "poll_interval_seconds": c.effective_poll_interval(cfg.poll_interval_seconds),
                "webhook_url": c.webhook_url,
                "webhook_secret_set": c.webhook_secret.is_some(),
                "rules": rules,
                "horizon_url": c.network.horizon_base_url(),
                "explorer_url": format!("{}/contract/{}", c.network.explorer_base_url().unwrap_or(""), c.contract_id),
                "webhooks": c.destinations().iter().map(destination_summary_json).collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::json!({
        "valid": true,
        "poll_interval_seconds": cfg.poll_interval_seconds,
        "cursor_file": cfg.cursor_file,
        "contracts": contracts,
    })
}

/// One-line description of a destination for `validate`. Secrets and routing
/// keys are shown only as set/none and header values are redacted.
fn describe_destination(destination: &WebhookDestination) -> String {
    let mut parts = vec![
        format!("format: {}", destination.format),
        format!(
            "secret: {}",
            if destination.secret.is_some() {
                "set"
            } else {
                "none"
            }
        ),
    ];
    if !destination.headers.is_empty() {
        parts.push(format!(
            "headers: {}",
            destination.headers.redacted().join(", ")
        ));
    }
    if destination.routing_key.is_some() {
        parts.push(format!("routing_key: {}", REDACTED));
    }
    if destination.client_cert.is_some() {
        parts.push("mtls: set".to_string());
    }
    if let Some(min) = &destination.min_severity {
        parts.push(format!("min_severity: {}", min));
    }
    if !destination.rule_types.is_empty() {
        parts.push(format!("rule_types: {}", destination.rule_types.join(", ")));
    }
    format!("{} ({})", destination.url, parts.join(", "))
}

/// `validate --format json` entry for a destination, with the same redaction.
fn destination_summary_json(destination: &WebhookDestination) -> serde_json::Value {
    let headers: serde_json::Map<String, serde_json::Value> = destination
        .headers
        .iter()
        .map(|(name, _)| (name.to_owned(), serde_json::Value::from(REDACTED)))
        .collect();
    serde_json::json!({
        "url": destination.url,
        "format": destination.format,
        "secret_set": destination.secret.is_some(),
        "headers": headers,
        "routing_key_set": destination.routing_key.is_some(),
        "mtls_set": destination.client_cert.is_some(),
        "min_severity": destination.min_severity,
        "rule_types": destination.rule_types,
    })
}

fn parse_webhook_format(value: &str) -> Result<WebhookFormat, String> {
    serde_json::from_value(serde_json::Value::from(value)).map_err(|_| {
        format!(
            "unknown format '{}': use txwatch, slack, discord, pagerduty or template",
            value
        )
    })
}

/// Parses repeated `--header "Name: value"` arguments.
fn parse_headers(raw: &[String]) -> Result<WebhookHeaders> {
    let mut headers = WebhookHeaders::default();
    for header in raw {
        let (name, value) = header
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("--header must look like \"Name: value\""))?;
        headers
            .0
            .insert(name.trim().to_owned(), value.trim().to_owned());
    }
    Ok(headers)
}

async fn check_webhook_reachable(client: &Client, url: &str) -> Result<&'static str> {
    let response = client.head(url).send().await;
    match response {
        Ok(resp) if resp.status().is_success() => Ok("reachable"),
        Ok(resp)
            if resp.status() == StatusCode::METHOD_NOT_ALLOWED
                || resp.status() == StatusCode::NOT_IMPLEMENTED =>
        {
            let resp = client.request(reqwest::Method::OPTIONS, url).send().await?;
            if resp.status().is_success() {
                Ok("reachable (OPTIONS)")
            } else {
                Ok("method not allowed")
            }
        }
        Ok(_) => Ok("method not allowed"),
        Err(err) => {
            if err.is_builder() {
                return Err(err.into());
            }
            Err(err.into())
        }
    }
}

struct HorizonCheck {
    network: String,
    status: &'static str,
    latest_ledger: Option<u64>,
    reachable: bool,
    found: bool,
    message: String,
}

async fn check_horizon_contract(
    client: &Client,
    contract: &txwatch_config::WatchedContract,
) -> HorizonCheck {
    let base = contract
        .horizon_base_url_override
        .as_deref()
        .unwrap_or_else(|| contract.network.horizon_base_url());
    let root = client.get(base).send().await;
    let (reachable, latest_ledger) = match root {
        Ok(response) if response.status().is_success() => {
            let json = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_default();
            (
                true,
                json.get("core_latest_ledger").and_then(|value| {
                    value
                        .as_u64()
                        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
                }),
            )
        }
        _ => (false, None),
    };
    if !reachable {
        return HorizonCheck {
            network: contract.network.as_str().into(),
            status: "unreachable",
            latest_ledger,
            reachable: false,
            found: false,
            message: format!("Horizon {} is unreachable", base),
        };
    }
    // Horizon exposes Soroban contract activity through the same account-style
    // transactions collection used by the poller. A 404 means the contract is
    // not known on this network; a successful empty collection is still valid.
    let found = client
        .get(format!(
            "{}/accounts/{}/transactions?limit=1",
            base, contract.contract_id
        ))
        .send()
        .await
        .map(|response| response.status().is_success())
        .unwrap_or(false);
    HorizonCheck {
        network: contract.network.as_str().into(),
        status: if found { "found" } else { "not found" },
        latest_ledger,
        reachable,
        found,
        message: if found {
            format!("{} exists on {}", contract.label, contract.network)
        } else {
            format!("{} not found on {}", contract.contract_id, contract.network)
        },
    }
}
// ── Tracing initialisation ────────────────────────────────────────────────────

fn init_tracing(format: LogFormat) {
    use tracing_subscriber::{fmt, EnvFilter};
    let builder = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false);
    match format {
        LogFormat::Text => builder.init(),
        // One JSON object per line, with the current span and the full span
        // list so fields such as `contract` and `tx` stay structured.
        LogFormat::Json => builder
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .init(),
    }
}
