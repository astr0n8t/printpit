//! Runs fake Moonraker instances for UI development (the same mock the tests use):
//!
//!   cargo run --example mock_moonraker
//!
//! 127.0.0.1:7201 is printing (with a camera), :7202 has Klipper shut down, :7203 has no
//! WebSocket (HTTP polling). Add them as printers with PRINTPIT_PRINTER_CIDRS=127.0.0.0/8.
//! (Not 7125: for Moonraker's default port, relative webcam URLs resolve to port 80, like a
//! real nginx-fronted install.)

#[allow(dead_code)]
#[path = "../tests/common/moonraker.rs"]
mod moonraker;

use moonraker::{MockMoonraker, MockOptions};

#[tokio::main]
async fn main() {
    MockMoonraker::start_on(7201, MockOptions::default()).await;
    MockMoonraker::start_on(
        7202,
        MockOptions {
            klippy_state: "shutdown",
            webcam: false,
            ..MockOptions::default()
        },
    )
    .await;
    MockMoonraker::start_on(
        7203,
        MockOptions {
            no_websocket: true,
            webcam: false,
            ..MockOptions::default()
        },
    )
    .await;
    println!("mock Moonraker on 127.0.0.1:7201 (printing), :7202 (shutdown), :7203 (no websocket)");
    std::future::pending::<()>().await;
}
