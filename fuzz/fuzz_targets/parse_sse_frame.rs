#![no_main]

use libfuzzer_sys::fuzz_target;
use txwatch_poller::event_stream::parse_sse_frame;

fuzz_target!(|frame: &str| {
    let _ = parse_sse_frame(frame);
});
