#![no_main]

#[path = "../../common/tests/fuzz/properties.rs"]
mod properties;

libfuzzer_sys::fuzz_target!(|data: &[u8]| properties::aead(data));
