#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    flow::fuzz_entry::run("input_values", data);
});
