//! Integration tests for the full slotbus request/response cycle.
//!
//! Each test creates its own SHM region with a unique name, so tests can run
//! in parallel. The `SlotBus` and `SlotWorker` are stopped at the end of each
//! test via their `stop()` method, which signals the response watcher and
//! receive loop threads to exit.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use slotbus::transport::Request;
use slotbus::types::RequestMeta;
use slotbus::{SlotBus, SlotBusConfig, SlotWorker};

/// Unique counter to avoid SHM name collisions between parallel tests.
static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Helper: create a SlotBus + SlotWorker pair with a unique name.
fn create_pair(test_name: &str, num_slots: usize) -> (SlotBus, Arc<SlotWorker>) {
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!("test-{}-{}-{}", test_name, std::process::id(), id);

    let config = SlotBusConfig::builder()
        .name(&name)
        .num_slots(num_slots)
        .wait_timeout_ms(500)
        .build();

    let bus = SlotBus::create(config).expect("failed to create SlotBus");

    let worker_config = SlotBusConfig::builder()
        .name(&name)
        .num_slots(num_slots)
        .wait_timeout_ms(500)
        .build();

    let worker = SlotWorker::open(worker_config).expect("failed to open SlotWorker");
    let worker = Arc::new(worker);

    (bus, worker)
}

/// Helper: build a simple RequestMeta for testing.
fn test_meta(path: &str) -> RequestMeta {
    RequestMeta {
        path: path.to_string(),
        route_pattern: path.to_string(),
        path_params: vec![],
        query: None,
        headers: vec![],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_request_response() {
    let (bus, worker) = create_pair("single", 4);
    bus.start_response_watcher();

    worker
        .clone()
        .start_receive_loop(move |w, slot, req: Request| {
            w.send_response(slot, 200, req.body, "text/plain", vec![])
                .unwrap();
        });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let meta = test_meta("/echo");
    let rx = bus
        .dispatch("req-1", "POST", &meta, b"hello")
        .expect("dispatch failed");

    let resp = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("timed out waiting for response")
        .expect("channel closed");

    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"hello");
    assert_eq!(resp.content_type, "text/plain");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequential_requests() {
    let (bus, worker) = create_pair("sequential", 4);
    bus.start_response_watcher();

    worker
        .clone()
        .start_receive_loop(move |w, slot, req: Request| {
            let body = req.path.into_bytes();
            w.send_response(slot, 200, body, "text/plain", vec![])
                .unwrap();
        });

    tokio::time::sleep(Duration::from_millis(50)).await;

    for i in 0..10 {
        let path = format!("/item/{i}");
        let meta = test_meta(&path);
        let req_id = format!("seq-{i}");

        let rx = bus
            .dispatch(&req_id, "GET", &meta, &[])
            .expect("dispatch failed");

        let resp = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("timed out")
            .expect("channel closed");

        assert_eq!(resp.status, 200);
        assert_eq!(String::from_utf8_lossy(&resp.body), path);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_responses() {
    let (bus, worker) = create_pair("concurrent", 32);
    bus.start_response_watcher();

    worker
        .clone()
        .start_receive_loop(move |w, slot, req: Request| {
            let mut resp = req.req_id.into_bytes();
            resp.push(b':');
            resp.extend_from_slice(&req.body);
            w.send_response(slot, 200, resp, "application/octet-stream", vec![])
                .unwrap();
        });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Dispatch all 20 requests serially (dispatch is synchronous and
    // find_free_slot is not thread-safe for concurrent callers), then
    // await all responses concurrently.
    let mut receivers = Vec::new();
    for i in 0..20 {
        let req_id = format!("conc-{i}");
        let path = format!("/concurrent/{i}");
        let meta = test_meta(&path);
        let body = format!("body-{i}");

        let rx = bus
            .dispatch(&req_id, "POST", &meta, body.as_bytes())
            .expect("dispatch failed");
        receivers.push((i, rx));
    }

    let mut handles = Vec::new();
    for (i, rx) in receivers {
        let handle = tokio::spawn(async move {
            let resp = tokio::time::timeout(Duration::from_secs(5), rx)
                .await
                .expect("timed out")
                .expect("channel closed");

            assert_eq!(resp.status, 200);

            let resp_str = String::from_utf8_lossy(&resp.body).to_string();
            let expected = format!("conc-{i}:body-{i}");
            assert_eq!(resp_str, expected);
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.await.expect("task panicked");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_body() {
    let (bus, worker) = create_pair("empty-body", 4);
    bus.start_response_watcher();

    worker
        .clone()
        .start_receive_loop(move |w, slot, req: Request| {
            assert!(req.body.is_empty());
            w.send_response(slot, 204, vec![], "text/plain", vec![])
                .unwrap();
        });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let meta = test_meta("/empty");
    let rx = bus
        .dispatch("req-empty", "DELETE", &meta, &[])
        .expect("dispatch failed");

    let resp = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("timed out")
        .expect("channel closed");

    assert_eq!(resp.status, 204);
    assert!(resp.body.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_body_overflow() {
    let (bus, worker) = create_pair("overflow", 4);
    bus.start_response_watcher();

    worker
        .clone()
        .start_receive_loop(move |w, slot, req: Request| {
            w.send_response(slot, 200, req.body, "application/octet-stream", vec![])
                .unwrap();
        });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // 512KB body — will use overflow regions since it exceeds the inline heap.
    let large_body: Vec<u8> = (0..512 * 1024).map(|i| (i % 256) as u8).collect();

    let meta = test_meta("/upload");
    let rx = bus
        .dispatch("req-large", "POST", &meta, &large_body)
        .expect("dispatch failed");

    let resp = tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .expect("timed out")
        .expect("channel closed");

    assert_eq!(resp.status, 200);
    assert_eq!(resp.body.len(), large_body.len());
    assert_eq!(resp.body, large_body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_headers_and_content_type() {
    let (bus, worker) = create_pair("headers", 4);
    bus.start_response_watcher();

    worker
        .clone()
        .start_receive_loop(move |w, slot, _req: Request| {
            let body = br#"{"ok": true}"#.to_vec();
            let headers = vec![
                ("x-request-id".into(), "test-123".into()),
                ("x-custom".into(), "value".into()),
            ];
            w.send_response(slot, 201, body, "application/json", headers)
                .unwrap();
        });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let meta = test_meta("/create");
    let rx = bus
        .dispatch("req-headers", "POST", &meta, b"{}")
        .expect("dispatch failed");

    let resp = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("timed out")
        .expect("channel closed");

    assert_eq!(resp.status, 201);
    assert_eq!(resp.content_type, "application/json");
    assert_eq!(resp.body, br#"{"ok": true}"#);
    assert_eq!(resp.headers.len(), 2);
    assert_eq!(resp.headers[0], ("x-request-id".into(), "test-123".into()));
    assert_eq!(resp.headers[1], ("x-custom".into(), "value".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_metadata_preserved() {
    let (bus, worker) = create_pair("metadata", 4);
    bus.start_response_watcher();

    worker
        .clone()
        .start_receive_loop(move |w, slot, req: Request| {
            let info = format!(
                "method={} path={} route={} params={:?} query={:?}",
                req.method, req.path, req.route_pattern, req.path_params, req.query,
            );
            w.send_response(slot, 200, info.into_bytes(), "text/plain", vec![])
                .unwrap();
        });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let meta = RequestMeta {
        path: "/users/42".to_string(),
        route_pattern: "/users/:id".to_string(),
        path_params: vec![("id".into(), "42".into())],
        query: Some("verbose=true".into()),
        headers: vec![("accept".into(), "application/json".into())],
    };

    let rx = bus
        .dispatch("req-meta", "GET", &meta, &[])
        .expect("dispatch failed");

    let resp = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("timed out")
        .expect("channel closed");

    let body = String::from_utf8_lossy(&resp.body).to_string();
    assert!(body.contains("method=GET"));
    assert!(body.contains("path=/users/42"));
    assert!(body.contains("route=/users/:id"));
    assert!(body.contains(r#""id": "42""#) || body.contains(r#""id", "42""#));
    assert!(body.contains("verbose=true"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_work_in_handler() {
    let (bus, worker) = create_pair("async-handler", 4);
    bus.start_response_watcher();

    // Capture the tokio runtime handle before starting the receive loop.
    // The handler runs on a blocking OS thread — it needs the handle
    // to spawn async work back onto the runtime.
    let rt_handle = tokio::runtime::Handle::current();

    worker
        .clone()
        .start_receive_loop(move |w, slot, req: Request| {
            let w = Arc::clone(&w);

            rt_handle.spawn(async move {
                // Simulate async processing.
                tokio::time::sleep(Duration::from_millis(5)).await;

                let body = format!("async response for {}", req.path);
                w.send_response(slot, 200, body.into_bytes(), "text/plain", vec![])
                    .unwrap();
            });
        });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let meta = test_meta("/async-test");
    let rx = bus
        .dispatch("req-async", "GET", &meta, &[])
        .expect("dispatch failed");

    let resp = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("timed out")
        .expect("channel closed");

    assert_eq!(resp.status, 200);
    assert_eq!(
        String::from_utf8_lossy(&resp.body),
        "async response for /async-test"
    );
}
