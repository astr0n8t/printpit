//! A fake Spoolman plus a fake Moonraker whose history grows, for working on the Spools tab:
//!
//!   cargo run --example mock_spoolman
//!
//! Spoolman is http://127.0.0.1:7912/spoolman (set PRINTPIT_SPOOLMAN_URL to that, no token).
//! The Moonraker on 127.0.0.1:7204 finishes a print every 20 s (so tracking books it on its own), alternating a one-tool job and a
//! two-tool one (6 g on T0, 2 g on T1). Add it as a printer with PRINTPIT_PRINTER_CIDRS=127.0.0.0/8
//! and turn tracking on from the Spools tab.

#[allow(dead_code)]
#[path = "../tests/common/moonraker.rs"]
mod moonraker;
#[allow(dead_code)]
#[path = "../tests/common/spoolman.rs"]
mod spoolman;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use moonraker::{MockMoonraker, MockOptions};
use serde_json::json;

#[tokio::main]
async fn main() {
    spoolman::MockSpoolman::start_on(7912).await;
    let printer = MockMoonraker::start_on(7204, MockOptions::default()).await;
    println!("mock Spoolman on http://127.0.0.1:7912/spoolman, mock Moonraker on 127.0.0.1:7204");
    let mut tick = tokio::time::interval(Duration::from_secs(20));
    tick.tick().await;
    let mut n = 0u64;
    loop {
        tick.tick().await;
        n += 1;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let job = if n % 2 == 1 {
            json!({"filename": "benchy.gcode", "status": "completed", "start_time": now - 10.0,
                   "end_time": now, "filament_used": 1520.5})
        } else {
            json!({"filename": "two-colour.gcode", "status": "cancelled", "start_time": now - 10.0,
                   "end_time": now, "filament_used": 2000.0,
                   "metadata": {"filament_total": 4000.0, "filament_weights": [6.0, 2.0]}})
        };
        let id = printer.add_job(job);
        // The dashboard (and printpit's tracking watcher) see the print end, then a new one start.
        printer.set_print_state("complete");
        println!("job {id} finished");
        tokio::time::sleep(Duration::from_secs(3)).await;
        printer.set_print_state("printing");
    }
}
