//! A fake Home Assistant for trying homeassistant-tui without a real instance.
//!
//!   cargo run --example mock_ha            # listens on 127.0.0.1:8124, token "demo"
//!   HA_URL=http://127.0.0.1:8124 HA_TOKEN=demo cargo run

#[path = "../tests/support/mock.rs"]
mod mock;

use std::time::Duration;

#[tokio::main]
async fn main() {
    let bind = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8124".into());
    let ha = mock::MockHa::start("demo", &bind).await;
    println!("mock Home Assistant on {} (token: demo)", ha.url());
    println!("  HA_URL={} HA_TOKEN=demo cargo run", ha.url());

    // Drift the temperature sensor so live updates are visible.
    let mut t = 21.3_f64;
    let mut step = 0.1;
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        t += step;
        if !(20.0..=23.0).contains(&t) {
            step = -step;
        }
        ha.set_state("sensor.living_temperature", &format!("{t:.1}"));
    }
}
