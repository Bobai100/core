# Fuzz targets

[`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) targets for TxWatch's hand-written
parsers of untrusted input. The crates expose the private parsers through a `fuzzing` feature
(`#[doc(hidden)] pub mod fuzzing`); it is not a public API.

| Target | Parser | Properties checked |
|---|---|---|
| `parse_stroops` | `txwatch-poller` amount parser | no panic; `"{whole}.{frac:07}"` round-trips to the same `u64` |
| `base64_decode` | `txwatch-poller` base64 decoder | no panic; decoding a standard encoding returns the input bytes |
| `contract_function_name` | `txwatch-poller` `ScVal::Symbol` XDR decoder | no panic |
| `interpolate` | `txwatch-config` `${VAR}` expansion | no panic; strings without `$` are unchanged |
| `parse_sse_frame` | `txwatch-poller` SSE frame parser | no panic |
| `validate_contract_id` | `txwatch-config` strkey validator | no panic; accepted IDs are 56 bytes |
| `glob_match` | `txwatch-rules` `*`/`?` matcher | no panic; `*` matches everything, a wildcard-free pattern matches itself |

## Running

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run parse_stroops            # until Ctrl-C
cargo +nightly fuzz run glob_match -- -max_total_time=60
```

CI runs each target for 30 seconds (`fuzz` job in `.github/workflows/ci.yml`).

## Findings

- `glob_match` is a backtracking matcher, so its run time is exponential in the number of `*`
  in a pattern. Patterns come from the operator's own config, not from untrusted input, so the
  target caps input sizes instead of treating this as a bug.
- No panics found so far. Record new findings here with the crashing input from
  `fuzz/artifacts/<target>/`, and add it as a regression test in the owning crate.
