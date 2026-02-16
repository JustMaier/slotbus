//! Embedded hub + worker in a single process.
//!
//! Demonstrates creating both sides of a slotbus connection for testing or
//! benchmarking. Shows the full request/response lifecycle with timing.
//!
//! # Running
//!
//! ```sh
//! cargo run --example embedded
//! ```
//!
//! No external hub needed — everything runs in-process.

use std::sync::Arc;
use std::time::Instant;

use slotbus::transport::Request;
use slotbus::types::RequestMeta;
use slotbus::{SlotBus, SlotBusConfig, SlotWorker};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("slotbus=debug")
        .init();

    // Use a unique name to avoid collisions with other running instances.
    let name = format!("embedded-demo-{}", std::process::id());

    // ---- Hub side: create the shared memory region ----
    let config = SlotBusConfig::builder()
        .name(&name)
        .num_slots(8)
        .build();

    let bus = SlotBus::create(config).expect("failed to create slotbus region");
    println!("created slotbus region: {}", bus.region_name());

    // Start the response watcher — it runs on a blocking thread, scanning
    // for Done slots and resolving pending oneshot channels.
    bus.start_response_watcher();

    // ---- Worker side: connect and start the receive loop ----
    let worker_config = SlotBusConfig::builder()
        .name(&name)
        .num_slots(8)
        .build();

    let worker = SlotWorker::open(worker_config).expect("failed to open slotbus region");
    let worker = Arc::new(worker);

    // The receive loop handler runs synchronously on a dedicated OS thread.
    // For async work, capture a tokio runtime handle and spawn from inside.
    worker.clone().start_receive_loop(move |w, slot, req: Request| {
        // Simulate some "processing" — in a real worker this might be a
        // database query, computation, or calling an external API.
        let response_body = format!(
            "Hello from embedded worker! You sent {} bytes to {} {}",
            req.body.len(),
            req.method,
            req.path,
        );

        w.send_response(
            slot,
            200,
            response_body.into_bytes(),
            "text/plain",
            vec![("x-worker".into(), "embedded".into())],
        )
        .expect("failed to send response");
    });

    // Give the worker thread a moment to start its event wait.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // ---- Send some requests and measure round-trip time ----
    println!("\nsending 5 requests...\n");

    for i in 0..5 {
        let meta = RequestMeta {
            path: format!("/test/{i}"),
            route_pattern: "/test/:id".into(),
            path_params: vec![("id".into(), i.to_string())],
            query: None,
            headers: vec![("content-type".into(), "text/plain".into())],
        };

        let body = format!("request body #{i}");
        let req_id = format!("req-{i}");

        let t0 = Instant::now();
        let rx = bus
            .dispatch(&req_id, "POST", &meta, body.as_bytes())
            .expect("dispatch failed");

        let response = rx.await.expect("response channel closed");
        let rtt = t0.elapsed();

        let body_str = String::from_utf8_lossy(&response.body);
        println!(
            "  [{i}] POST {} -> {} ({}) in {:.1}ms",
            meta.path,
            response.status,
            body_str,
            rtt.as_secs_f64() * 1000.0,
        );
    }

    println!("\ndone! all requests completed successfully.");
}
