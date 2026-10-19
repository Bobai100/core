#![no_main]

use libfuzzer_sys::fuzz_target;
use txwatch_config::fuzzing::interpolate;

fuzz_target!(|data: (&str, &str)| {
    let (value, var) = data;
    let lookup = |name: &str| (!name.is_empty()).then(|| var.to_owned());
    let _ = interpolate(value, &lookup);

    // Strings without `$` are returned unchanged.
    if !value.contains('$') {
        assert_eq!(interpolate(value, &lookup).as_deref(), Some(value));
    }
});
