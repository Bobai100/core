#![no_main]

use libfuzzer_sys::fuzz_target;
use txwatch_poller::fuzzing::contract_function_name;

fuzz_target!(|value: &str| {
    let _ = contract_function_name(value);
});
