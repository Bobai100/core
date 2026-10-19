#![no_main]

use libfuzzer_sys::fuzz_target;
use txwatch_rules::fuzzing::glob_match;

fuzz_target!(|data: (&str, &str)| {
    let (pattern, text) = data;
    // Bound input size: matching is exponential in the number of `*`.
    if pattern.len() > 64 || text.len() > 256 {
        return;
    }
    let _ = glob_match(pattern, text);

    assert!(glob_match("*", text));
    if !text.contains(['*', '?']) {
        assert!(glob_match(text, text));
    }
});
