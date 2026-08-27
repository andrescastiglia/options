#![no_main]

use libfuzzer_sys::fuzz_target;
use options_trading::market::{CapturedMarketFrame, MarketFrame, ReplayMarket};

fuzz_target!(|data: &[u8]| {
    let _ = ReplayMarket::from_jsonl_bytes(data);
    if let Ok(frame) = serde_json::from_slice::<MarketFrame>(data) {
        let _ = ReplayMarket::new(vec![frame]);
    }
    if let Ok(capture) = serde_json::from_slice::<CapturedMarketFrame>(data) {
        let _ = capture.validate();
    }
});
