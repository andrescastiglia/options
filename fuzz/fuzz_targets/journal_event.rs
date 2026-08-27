#![no_main]

use libfuzzer_sys::fuzz_target;
use options_trading::persistence::{validate_event_identity, JournalEvent};

fuzz_target!(|data: &[u8]| {
    if let Ok(event) = serde_json::from_slice::<JournalEvent>(data) {
        let _ = validate_event_identity(&event);
    }
});
