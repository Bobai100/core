#![no_main]

use libfuzzer_sys::fuzz_target;
use txwatch_poller::fuzzing::base64_decode;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fuzz_target!(|data: (&str, &[u8])| {
    let (input, bytes) = data;
    let _ = base64_decode(input);

    // Round trip: decoding a standard encoding returns the original bytes.
    assert_eq!(base64_decode(&encode(bytes)).as_deref(), Some(bytes));
});
