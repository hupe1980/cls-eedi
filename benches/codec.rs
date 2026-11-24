//! Run with cargo bench --bench codec. Reports per-call elapsed time in nanoseconds.
use cls_eedi::wire::Message;
use serde_json::json;
use std::{hint::black_box, time::Instant};

fn main() {
    for count in [1, 32, 128] {
        let meters: Vec<_> = (0..count)
            .map(|i| {
                json!({
                    "id": format!("meter-{i}"), "source": "gcp",
                    "power": {"total": {"value": {"number": -237, "scale": 1}}}
                })
            })
            .collect();
        let bytes = serde_json::to_vec(&json!({
            "type": "de.keo-connectivity.clseedi.state", "source": "bench", "id": "sample",
            "specversion": "1.0", "data": {"protocol":"1.2.0","timestamp":100,"measurements":meters}
        }))
        .unwrap();
        for _ in 0..32 {
            black_box(Message::decode(black_box(&bytes)).unwrap());
        }
        let mut samples = Vec::with_capacity(2000);
        for _ in 0..2000 {
            let start = Instant::now();
            black_box(Message::decode(black_box(&bytes)).unwrap());
            samples.push(start.elapsed().as_nanos());
        }
        samples.sort_unstable();
        println!(
            "{count} meters, {} bytes, decode p50={} ns p99={} ns",
            bytes.len(),
            samples[999],
            samples[1979]
        );
    }
}
