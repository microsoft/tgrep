#![no_main]

use libfuzzer_sys::fuzz_target;
use tgrep_core::query;

fuzz_target!(|data: &[u8]| {
    // Only fuzz valid UTF-8 strings as regex patterns.
    let Ok(pattern) = std::str::from_utf8(data) else {
        return;
    };

    // build_query_plan must not panic — it may return Err for invalid regex.
    let _ = query::build_query_plan(pattern, false);
    let _ = query::build_query_plan(pattern, true);

    // build_literal_plan must not panic on any string.
    let _ = query::build_literal_plan(pattern, false);
    let _ = query::build_literal_plan(pattern, true);
});
