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
    #[arg(long, global = true, value_parser = parse_horizon_url)]
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

/// Network names accepted by `test-webhook --network`. `public` and `pubnet`
/// are the names Stellar Expert uses for mainnet.
#[derive(Clone, Copy, ValueEnum)]
enum NetworkArg {
    Mainnet,
    #[value(alias = "public", alias = "pubnet")]
    MainnetAlias,
    Testnet,
    Futurenet,
}

impl NetworkArg {
    fn to_network(self) -> txwatch_config::Network {
        match self {
            NetworkArg::Mainnet | NetworkArg::MainnetAlias => txwatch_config::Network::Mainnet,
            NetworkArg::Testnet => txwatch_config::Network::Testnet,
            NetworkArg::Futurenet => txwatch_config::Network::Futurenet,
        }
    }
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

        /// Maximum number of contracts polled concurrently
        #[arg(long, default_value_t = 10)]
        concurrency: usize,
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

        /// Stellar network for the test payload
        #[arg(long, value_enum, default_value = "testnet")]
        network: NetworkArg,

        /// Horizon base URL for a custom network in the test payload
        #[arg(long, value_parser = parse_horizon_url)]
        horizon_url: Option<String>,

        /// Explorer base URL for a custom network in the test payload
        #[arg(long, value_parser = parse_horizon_url)]
        explorer_url: Option<String>,

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

    /// Remove saved cursor entries for contracts that are no longer configured
    ///
    /// Reloads keep cursors of removed contracts so re-adding one resumes where
    /// it left off; use this command to intentionally discard them.
    PruneCursors {
        /// Only report what would be removed, without writing the cursor file
        #[arg(long)]
        dry_run: bool,
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
                println!("    horizon      : {}", poll_base_url(c));
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
            horizon_url,
            explorer_url,
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
                    find_contract_by_label(&cfg, wanted)
                })
                .transpose()?;
            let (destinations, network, horizon, explorer) = if let Some(c) = configured {
                let mut destinations = c.destinations();
                if secret.is_some() {
                    for destination in &mut destinations {
                        destination.secret = secret.clone();
                    }
                }
                let horizon = poll_base_url(&c).to_string();
                let explorer = c.network.explorer_base_url().map(str::to_string);
                (destinations, c.network, horizon, explorer)
            } else {
                let selected = network.to_network();
                let horizon = horizon_url
                    .clone()
                    .unwrap_or_else(|| selected.horizon_base_url().to_string());
                let explorer = explorer_url
                    .clone()
                    .or_else(|| selected.explorer_base_url().map(str::to_string));
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
                (vec![destination], selected, horizon, explorer)
            };
            let client = build_client().context("failed to build HTTP client")?;

            // Try every destination, then fail if any of them failed.
            let mut failed = 0;
            for destination in &destinations {
                let payload = test_payload_with_network(
                    &label,
                    network.as_str(),
                    &horizon,
                    explorer.as_deref(),
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
            // Write to a sibling temp file and rename over the target so an
            // interrupted write never leaves a truncated config. `fs::rename`
            // replaces an existing file atomically on Unix and on Windows
            // (via MoveFileEx with MOVEFILE_REPLACE_EXISTING).
            let tmp = output.with_extension("toml.tmp");
            fs::write(&tmp, raw)
                .with_context(|| format!("failed to write '{}'", tmp.display()))?;
            fs::rename(&tmp, output).with_context(|| {
                let _ = fs::remove_file(&tmp);
                format!("failed to write '{}'", output.display())
            })?;
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
            let contract = find_contract_by_label(&cfg, contract)?;
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

        Command::PruneCursors { dry_run } => {
            let cfg = load_config(&cli)?;
            let removed = prune_cursors(&cfg.cursor_file, &cfg, dry_run)?;
            if removed.is_empty() {
                println!("No stale cursor entries found in '{}'.", cfg.cursor_file);
            } else {
                let verb = if dry_run { "Would remove" } else { "Removed" };
                println!(
                    "{} {} stale cursor entr{} from '{}':",
                    verb,
                    removed.len(),
                    if removed.len() == 1 { "y" } else { "ies" },
                    cfg.cursor_file
                );
                for key in &removed {
                    println!("  - {}", key);
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
            concurrency,
            #[cfg(feature = "metrics")]
            metrics_addr,
        } => {
            let config_path = required_config(&cli.config)?;
            let cfg = load_config(&cli)?;
            let concurrency = concurrency.max(1);

            if once {
                info!(
                    version = VERSION,
                    contracts = cfg.contracts.len(),
                    dry_run,
                    concurrency,
                    "running a single TxWatch poll cycle"
                );
                let report = txwatch_poller::run_once(cfg, dry_run, concurrency).await?;
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
            spawn_signal_handlers(shutdown_tx);

            #[cfg(feature = "metrics")]
            if let Some(addr) = metrics_addr {
                // Register before serving so the first scrape already has it.
                txwatch_poller::metrics::register_build_info();
                txwatch_poller::serve_metrics(addr, shutdown_rx.clone()).await?;
            }
            // On Unix this installs a SIGHUP handler; on Windows it is a no-op.
            let (reload_tx, reload_rx) = tokio::sync::mpsc::channel(1);
            spawn_reload_on_sighup(config_path, cli.horizon_url.clone(), reload_tx)?;

            info!(
                version = VERSION,
                contracts = cfg.contracts.len(),
                interval_secs = cfg.poll_interval_seconds,
                dry_run = dry_run,
                concurrency,
                "starting TxWatch"
            );
            txwatch_poller::run_with_reload(cfg, dry_run, concurrency, shutdown_rx, reload_rx)
                .await?;
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

/// Forward SIGINT, SIGTERM and SIGQUIT into `shutdown_tx` so containers and
/// systemd (`docker stop`, Kubernetes) shut down cleanly. A second SIGINT
/// forces an immediate exit.
#[cfg(unix)]
fn spawn_signal_handlers(shutdown_tx: watch::Sender<bool>) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = ?e, "failed to install SIGINT handler");
            return;
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = ?e, "failed to install SIGTERM handler");
            return;
        }
    };
    let mut sigquit = match signal(SignalKind::quit()) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = ?e, "failed to install SIGQUIT handler");
            return;
        }
    };

    tokio::spawn(async move {
        let mut shutting_down = false;
        loop {
            let is_sigint = tokio::select! {
                _ = sigint.recv() => true,
                _ = sigterm.recv() => false,
                _ = sigquit.recv() => false,
            };
            if shutting_down {
                if is_sigint {
                    error!("second SIGINT received — exiting immediately, state may not be flushed");
                    std::process::exit(130);
                }
                continue;
            }
            shutting_down = true;
            info!("shutting down, press Ctrl-C again to force");
            let _ = shutdown_tx.send(true);
        }
    });
}

#[cfg(not(unix))]
fn spawn_signal_handlers(shutdown_tx: watch::Sender<bool>) {
    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            warn!(error = ?e, "failed to install Ctrl+C handler");
            return;
        }
        info!("shutting down, press Ctrl-C again to force");
        let _ = shutdown_tx.send(true);
    });
}

/// On SIGHUP, re-read and validate the config file and hand it to the poller.
/// An invalid file is logged and ignored, so the previous config keeps running.
#[cfg(unix)]
fn spawn_reload_on_sighup(
    path: PathBuf,
    horizon_url: Option<String>,
    reload_tx: tokio::sync::mpsc::Sender<AppConfig>,
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

/// Windows has no SIGHUP, so config reloading is a no-op there. The poller
/// still runs normally; restart the process to pick up a changed config.
#[cfg(not(unix))]
fn spawn_reload_on_sighup(
    _path: PathBuf,
    _horizon_url: Option<String>,
    _reload_tx: tokio::sync::mpsc::Sender<AppConfig>,
) -> Result<()> {
    info!("SIGHUP config reload is not supported on this platform; restart to reload");
    Ok(())
}

/// Re-read and validate the config file. Only used by the Unix SIGHUP handler,
/// so it is dead code on other platforms.
#[cfg_attr(not(unix), allow(dead_code))]
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

/// Remove cursor entries whose `<network>:<contract_id>` key matches no
/// configured contract. Returns the removed keys. When `dry_run` is set the
/// file is left untouched.
fn prune_cursors(path: &str, cfg: &AppConfig, dry_run: bool) -> Result<Vec<String>> {
    let path = Path::new(path);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read cursor file '{}'", path.display()))?;
    let mut cursors: std::collections::BTreeMap<String, String> = if raw.trim().is_empty() {
        std::collections::BTreeMap::new()
    } else {
        serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse cursor file '{}'", path.display()))?
    };

    let configured: std::collections::HashSet<String> = cfg
        .contracts
        .iter()
        .map(|c| format!("{}:{}", c.network.as_str(), c.contract_id))
        .collect();

    let stale: Vec<String> = cursors
        .keys()
        .filter(|key| !configured.contains(*key))
        .cloned()
        .collect();

    if !dry_run && !stale.is_empty() {
        for key in &stale {
            cursors.remove(key);
        }
        let serialized = serde_json::to_string_pretty(&cursors)?;
        // Write to a sibling temp file and rename so a crash can't truncate it.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, serialized)
            .with_context(|| format!("failed to write '{}'", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("failed to replace '{}'", path.display()))?;
    }

    Ok(stale)
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

/// The Horizon base URL actually used to poll `contract`: the per-contract
/// override (including `--horizon-url`) when set, otherwise the network default.
fn poll_base_url(contract: &txwatch_config::WatchedContract) -> &str {
    contract
        .horizon_base_url_override
        .as_deref()
        .unwrap_or_else(|| contract.network.horizon_base_url())
}

/// Find a configured contract by label, matching case-insensitively after
/// trimming (config validation treats such labels as duplicates). On a miss the
/// error lists the configured labels.
fn find_contract_by_label(
    cfg: &AppConfig,
    wanted: &str,
) -> Result<txwatch_config::WatchedContract> {
    let wanted = wanted.trim();
    cfg.contracts
        .iter()
        .find(|c| c.label.trim().eq_ignore_ascii_case(wanted))
        .cloned()
        .ok_or_else(|| {
            let labels: Vec<&str> = cfg.contracts.iter().map(|c| c.label.as_str()).collect();
            if labels.is_empty() {
                anyhow::anyhow!(
                    "configured contract '{}' not found (no contracts are configured)",
                    wanted
                )
            } else {
                anyhow::anyhow!(
                    "configured contract '{}' not found; configured labels: {}",
                    wanted,
                    labels.join(", ")
                )
            }
        })
}

/// Validate a `--horizon-url` / `--explorer-url` value: it must be an absolute
/// http(s) URL. Trailing slashes are trimmed so the stored value is canonical.
fn parse_horizon_url(value: &str) -> Result<String, String> {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("URL must not be empty".to_string());
    }
    if let Err(e) = txwatch_config::check_http_url(trimmed) {
        return Err(format!("invalid URL '{}': {:#}", value, e));
    }
    Ok(trimmed.to_string())
}

/// Machine-readable `validate` summary. Webhook secrets are never printed;
/// only whether one is set.
fn config_summary_json(cfg: &AppConfig) -> serde_json::Value {
    let contracts: Vec<_> = cfg
        .contracts
        .iter()
        .map(|c| {
            let rules: Vec<_> = c
                .rules
                .iter()
                .map(|entry| {
                    let mut obj = serde_json::to_value(&entry.rule)
                        .unwrap_or_else(|_| serde_json::json!({}));
                    if let Some(cooldown) = entry.cooldown_seconds {
                        if let Some(map) = obj.as_object_mut() {
                            map.insert(
                                "cooldown_seconds".to_string(),
                                serde_json::json!(cooldown),
                            );
                        }
                    }
                    obj
                })
                .collect();
            let horizon_url = c
                .horizon_base_url_override
                .as_deref()
                .unwrap_or_else(|| c.network.horizon_base_url());
            serde_json::json!({
                "label": c.label,
                "contract_id": c.contract_id,
                "network": c.network.as_str(),
                "enabled": c.enabled,
                "poll_interval_seconds": c.effective_poll_interval(cfg.poll_interval_seconds),
                "webhook_url": c.webhook_url,
                "webhook_secret_set": c.webhook_secret.is_some(),
                "rules": rules,
                "horizon_url": horizon_url,
                "explorer_url": c.network.explorer_base_url().map(|e| format!("{}/contract/{}", e, c.contract_id)),
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
    let base = poll_base_url(contract);
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
