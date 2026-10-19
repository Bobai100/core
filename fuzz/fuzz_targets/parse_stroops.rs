#![no_main]

use libfuzzer_sys::fuzz_target;
use txwatch_poller::fuzzing::parse_stroops;

fuzz_target!(|data: (&str, u64)| {
    let (input, n) = data;
    let _ = parse_stroops(input);

    // Round trip: any stroop value formatted the way Horizon does parses back exactly.
    let formatted = format!("{}.{:07}", n / 10_000_000, n % 10_000_000);
    assert_eq!(parse_stroops(&formatted), Some(n), "{formatted}");
});
