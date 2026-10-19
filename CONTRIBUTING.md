# Contributing to TxWatch core

## Local Development with Docker Compose

### Prerequisites

- Docker and Docker Compose installed

### Getting started

Start the full stack (txwatch + webhook echo server):

```bash
docker compose up
```

This runs:
- **txwatch** — the main polling service, using `config/docker-compose.toml`: it watches the Testnet native XLM contract (always active) and sends every alert to the echo server
- **webhook** — a local echo server listening on `http://localhost:8080` that logs all incoming POST requests

### Viewing webhook payloads

All webhook calls from txwatch are logged by the echo server. Watch the output in your terminal:

```
webhook   | {"timestamp":"2025-02-28T...", "method":"POST", "url":"/hook", "body":{...}}
```

To check delivery end to end without waiting for a transaction, send a test
payload from the txwatch image to the echo server:

```bash
docker compose run --rm txwatch test-webhook --url http://webhook:8080/hook
```

You can also inspect payloads by manually curling the webhook:

```bash
curl -X POST http://localhost:8080/webhook -H "Content-Type: application/json" -d '{"test": "payload"}'
```

### Editing the config

To watch a different contract or change alert rules:

1. Edit `config/docker-compose.toml` (this is the file mounted by `docker-compose.yml`)
2. Restart the stack: `docker compose down && docker compose up`

Alternatively, send a `SIGHUP` to the running container to reload the config
without restarting the stack:

```bash
docker compose kill -s HUP txwatch
```

### Stopping the stack

```bash
docker compose down
```

---

## Sister repos

| Repo | Description |
|------|-------------|
| [web](https://github.com/Tx-wats/web) | Web dashboard for alert history |
| [contracts](https://github.com/Tx-wats/contracts) | Example Soroban contracts to monitor |

---

## Local dev setup

### 1. Rust toolchain

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup update stable
```

Requires Rust stable ≥ 1.88 (the `rust-version` declared in the workspace `Cargo.toml`, checked by the MSRV CI job).

### 2. Clone and build

```bash
git clone https://github.com/Tx-wats/core
cd core
cargo build
```

### 3. Validate the example config

```bash
cargo run -p txwatch -- --config config/example.toml validate
```

### 4. Get a testnet contract to watch

The easiest way is to deploy one of the contracts from
[Tx-wats/contracts](https://github.com/Tx-wats/contracts),
or use any existing Soroban contract on Testnet.

You can find active testnet contracts on the
[Stellar Expert Testnet explorer](https://stellar.expert/explorer/testnet).

Copy the contract's C-address (56 characters, starts with `C`) into your config:

```toml
[[contracts]]
label       = "My Test Contract"
contract_id = "CXXX..."   # paste your contract address here
network     = "testnet"
webhook_url = "https://webhook.site/your-unique-id"  # use webhook.site for testing
```

### 5. Start watching

```bash
RUST_LOG=debug cargo run -p txwatch -- --config config/my-config.toml watch
```

---

## Running Tests

```bash
cargo test --workspace
```

This runs all tests across all crates. For testing a specific crate:

```bash
cargo test -p txwatch-config
cargo test -p txwatch-rules
cargo test -p txwatch-notifier
cargo test -p txwatch-poller   # includes integration tests
```

Add `-- --nocapture` to see log output while tests run.

## Adding Tests

### Where to put tests

- **Unit tests:** in the same file as the code under test, in a `#[cfg(test)]` module at the bottom
- **Integration tests:** in a `tests/` directory at the crate root

### Using wiremock

The project uses [wiremock](https://crates.io/crates/wiremock) to mock HTTP endpoints in integration tests — it spins up local HTTP servers that simulate Horizon and webhook endpoints without requiring network access. See `crates/poller/tests/integration.rs` for examples of how to use it.

---

## Dependency lock files

`Cargo.lock` is committed to this repository. This workspace contains both
a binary crate (`crates/cli`) and library crates, and we commit the lock
file to ensure reproducible builds of the binary.

**When submitting a pull request:**

- If you update dependencies (directly or indirectly), commit the updated `Cargo.lock`
- Use `cargo update` to update dependencies, then commit the resulting lock file
- Do not remove or exclude `Cargo.lock` from commits

This ensures all contributors and CI builds use the exact same dependency versions.

---

## How to add a new `AlertRule` type

Follow these steps in order. Each step is in a different file.

### Step 1 — Declare the variant (`crates/config/src/lib.rs`)

Add your variant to the `AlertRule` enum:

```rust
pub enum AlertRule {
    // ... existing variants ...
    MyNewRule { my_field: String },
}
```

### Step 2 — Validate the new fields (`crates/config/src/lib.rs`)

Add a match arm in `AlertRule::validate()`:

```rust
AlertRule::MyNewRule { my_field } => {
    if my_field.trim().is_empty() {
        bail!("contract '{}': MyNewRule my_field must not be empty", contract_label);
    }
}
```

### Step 3 — Evaluate the rule (`crates/rules/src/lib.rs`)

Add a match arm in `eval_rule()`:

```rust
AlertRule::MyNewRule { my_field } => {
    Ok(tx.function_name.as_deref() == Some(my_field.as_str()))
}
```

> Note: Adding a new `AlertRule` variant requires updating both `eval_rule()` and
> `rule_label()` in `crates/rules/src/lib.rs`. Rust's exhaustive matching helps
> catch missing arms, but both functions must remain in sync for new variants.

### Step 4 — Label the rule (`crates/rules/src/lib.rs`)

Add a match arm in `rule_label()`:

```rust
AlertRule::MyNewRule { my_field } => format!("MyNewRule({})", my_field),
```

### Step 5 — Add to the TOML config format

Users configure it like:

```toml
[[contracts.rules]]
type     = "MyNewRule"
my_field = "some_value"
```

### Step 6 — Write tests

Add unit tests in `crates/rules/src/lib.rs` and optionally an integration
test in `crates/poller/tests/integration.rs`.

### Step 7 — Document it

Add an entry to `docs/alert-rules.md`.

---

## Code style

```bash
cargo fmt          # format
cargo clippy -- -D warnings   # lint (must pass clean)
```

## Commit style

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(rules): add MyNewRule evaluation
fix(config): reject empty contract labels
test(poller): add integration test for MyNewRule
docs: document MyNewRule in alert-rules.md
```

## PR checklist

- [ ] `cargo test` passes
- [ ] `cargo clippy -- -D warnings` passes
- [ ] `cargo fmt` applied
- [ ] New rule documented in `docs/alert-rules.md`
- [ ] CONTRIBUTING.md updated if the contribution process changed
- [ ] CHANGELOG.md entry added under [Unreleased] describing what changed

## Branch protection and required CI checks

`main` is protected. A pull request can only be merged once **all** of the
following required status checks pass:

- `fmt`
- `clippy`
- `test`
- `msrv`
- `deny`
- `docker`

Branch protection also requires branches to be **up to date with `main`**
before merging, so rebase or merge `main` into your branch if it falls behind.

Never merge a PR — your own or a bot's — while any required check is failing
or still running.

## Dependabot review/merge policy

Dependabot PRs are subject to exactly the same rules as human PRs:

- **Do not merge a Dependabot PR while any required CI check is failing.**
  A red `fmt`, `clippy`, `test`, `msrv`, `deny`, or `docker` check blocks the
  merge, even for a patch-level dependency bump.
- **Branches must be up to date with `main` before merging.** If `main` has
  moved on, ask Dependabot to rebase (`@dependabot rebase`) and wait for CI to
  run again on the updated branch.
- Branch protection on `main` enforces these required status checks, so the
  merge button stays disabled until they are green — do not use admin
  overrides to bypass them.
- If a Dependabot PR cannot be made green (e.g. an upstream release breaks the
  MSRV or a lint), close it and open a tracking issue instead of merging it.
=======
