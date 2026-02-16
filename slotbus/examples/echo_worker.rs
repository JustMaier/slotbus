//! Minimal echo worker example.
//!
//! Connects to a slotbus region created by a hub (e.g. `slotbus-hub`) and
//! echoes every request body back as the response.
//!
//! # Running
//!
//! 1. Start a hub that creates a region named "echo":
//!    ```sh
//!    cargo run -p slotbus-hub -- --port 3200
//!    ```
//!    Then register a worker named "echo" via the hub's API.
//!
//! 2. Run this example:
//!    ```sh
//!    cargo run --example echo_worker
//!    ```
//!
//! The worker will block on the receive loop, waiting for requests from the hub.

use std::sync::Arc;

use slotbus::transport::Request;
use slotbus::{SlotBusConfig, SlotWorker};

fn main() {
    // Initialize tracing so you can see slotbus debug logs.
    tracing_subscriber::fmt()
        .with_env_filter("slotbus=debug")
        .init();

    // Build the config. The name must match what the hub created.
    let config = SlotBusConfig::builder().name("echo").build();

    // Open the existing shared memory region + events.
    // This will fail if no hub has created a region named "slotbus-echo" yet.
    let worker =
        SlotWorker::open(config).expect("failed to open slotbus region — is the hub running?");
    let worker = Arc::new(worker);

    println!("echo worker connected to slotbus region");
    println!("waiting for requests... (press Ctrl+C to stop)");

    // Start the blocking receive loop. The closure is called synchronously
    // on the receive thread for each incoming request.
    let handle = worker
        .clone()
        .start_receive_loop(move |w, slot, req: Request| {
            println!(
                "[slot {}] {} {} ({} bytes)",
                slot,
                req.method,
                req.path,
                req.body.len(),
            );

            // Echo the request body back as the response.
            let status = 200;
            let body = req.body;
            let content_type = "application/octet-stream";

            w.send_response(slot, status, body, content_type, vec![])
                .expect("failed to send response");
        });

    // The receive loop runs forever. Join it so main doesn't exit.
    handle.join().expect("receive loop panicked");
}
