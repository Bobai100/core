#![no_main]

use libfuzzer_sys::fuzz_target;
use txwatch_config::validate_contract_id;

fuzz_target!(|id: &str| {
    if validate_contract_id(id).is_ok() {
        // Every accepted ID is a 56-character strkey.
        assert_eq!(id.len(), 56);
    }
});
