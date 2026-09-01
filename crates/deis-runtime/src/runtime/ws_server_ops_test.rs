// WebSocket Server Ops unit tests

#[cfg(test)]
mod tests {
    use crate::runtime::ops::{init_ops_extension, ResponseStore};
    use crate::runtime::ws_server_ops::{create_ws_server_manager, init_ws_server_extension};
    use crate::runtime::ServiceRouter;
    use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions};
    use std::cell::RefCell;
    // use std::collections::HashMap;
    use std::rc::Rc;

    fn setup_test_runtime() -> JsRuntime {
        let runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_ops_extension(), init_ws_server_extension()],
            ..Default::default()
        });

        // Inject necessary Stores
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        let response_store: ResponseStore = Rc::new(RefCell::new(None));
        state.put(response_store);
        state.put(create_ws_server_manager());

        runtime
    }

    #[test]
    fn test_ws_server_accept() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            const result = Deno.core.ops.op_ws_server_accept("test-ws-1");
            if (result.success === true) {
                "ok";
            } else {
                throw new Error("Expected success");
            }
        "#;

        runtime.execute_script("<test>", code).unwrap();
    }

    #[tokio::test]
    async fn test_ws_server_send_nonexistent() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                const result = await Deno.core.ops.op_ws_server_send("nonexistent-ws", "hello");
                if (result.success === false && result.error.includes("not found")) {
                    return "ok";
                }
                throw new Error("Expected error about connection not found");
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_ws_server_close_nonexistent() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                const request = {
                    ws_id: "nonexistent-ws",
                    code: 1000,
                    reason: "test close"
                };
                const result = await Deno.core.ops.op_ws_server_close(JSON.stringify(request));
                if (result.success === false && result.error.includes("not found")) {
                    return "ok";
                }
                throw new Error("Expected error about connection not found");
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();
    }

    /// A binary frame an application sends reaches the bridge as bytes.
    ///
    /// `WsServerMessage::Binary` was a variant nothing in the crate ever
    /// constructed: `WebSocketServer.send` put everything that was not a
    /// string through `JSON.stringify`, so `send(new Uint8Array([1, 2, 3]))`
    /// left the node as the TEXT frame `{"0":1,"1":2,"2":3}`. This drives the
    /// op an application now reaches and reads what came out the other end of
    /// the channel the bridge holds.
    #[tokio::test]
    async fn a_binary_frame_leaves_the_isolate_as_bytes() {
        use crate::runtime::WsServerMessage;

        let mut runtime = setup_test_runtime();

        // Stand in for the bridge: whatever the op sends arrives here.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // The clone is taken and the borrow dropped before the await: an
        // `OpState` borrow held across one would still be held when the
        // isolate next reaches for it.
        let manager = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<crate::runtime::WsServerManager>().clone()
        };
        manager.write().await.insert(
            "bound-ws".to_string(),
            crate::runtime::WsClientSink::new(tx, std::sync::Arc::new(0.into())),
        );

        let code = r#"
            (async () => {
                // 0x00 and 0xff are the two a JSON round trip could not carry.
                const frame = new Uint8Array([0, 1, 2, 255]);
                const result = await Deno.core.ops.op_ws_server_send_binary("bound-ws", frame);
                if (result.success !== true) {
                    throw new Error("send failed: " + result.error);
                }
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();

        match rx.recv().await.expect("the bridge received a frame") {
            WsServerMessage::Binary(bytes) => assert_eq!(bytes, vec![0u8, 1, 2, 255]),
            other => panic!("a binary send arrived at the bridge as {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_binary_send_to_a_connection_that_is_gone_says_so() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                const result = await Deno.core.ops.op_ws_server_send_binary(
                    "nonexistent-ws", new Uint8Array([1]));
                if (result.success === false && result.error.includes("not found")) {
                    return "ok";
                }
                throw new Error("Expected error about connection not found");
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();
    }

    /// The WebSocket classes are reachable from an application.
    ///
    /// Driven through a real `AppRuntime` rather than by evaluating
    /// `js/runtime.js` on a bare `JsRuntime`, which is what this used to do.
    /// That shortcut stopped working once the standard web globals were bound
    /// from the deno extensions: `js/runtime.js` now subclasses the `Response`
    /// that `js/bootstrap.js` binds, so evaluating it in an isolate that was
    /// built without those extensions fails on the first class it declares.
    /// Going through `AppRuntime` also means this asserts what an application
    /// actually sees, rather than what one particular script leaves behind.
    #[tokio::test]
    async fn test_ws_server_api_availability() {
        use crate::runtime::isolate::{AppRuntime, IsolateSpec};
        use crate::runtime::ServiceWiring;
        use std::collections::HashMap;

        let code = r#"
            export default function handler() {
                if (typeof WebSocketServer === "undefined") {
                    throw new Error("WebSocketServer not available");
                }
                if (typeof WebSocketPair === "undefined") {
                    throw new Error("WebSocketPair not available");
                }
                return new Response("ok");
            }
        "#;

        let mut runtime = AppRuntime::new(
            "ws_api_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (status, _headers, body) = rx.await.unwrap().unwrap();
        assert_eq!(status, 200, "{body:?}");
        assert_eq!(body.into_bytes().as_deref(), Some(&b"ok"[..]));
    }

    /// What a handler that outruns its client costs the node.
    ///
    /// The queue between the isolate and one client's socket is Rust memory
    /// beside the isolate, outside the V8 heap `memoryMb` governs, and it had
    /// no ceiling of any kind: `send()` handed the frame over and returned, so
    /// a handler sending faster than one client reads grew the node's process
    /// without ever crossing its own memory limit -- the same shape the Cache
    /// API had. The outbound `WebSocket` client already answered this one; the
    /// server side now gives the same answer.
    #[tokio::test]
    async fn a_client_that_is_behind_is_told_rather_than_buffered_without_end() {
        use crate::runtime::{WsClientSink, WsServerMessage};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let mut runtime = setup_test_runtime();

        let (frames, mut bridge) = tokio::sync::mpsc::unbounded_channel();
        let buffered = Arc::new(AtomicUsize::new(0));
        let (reports, mut report) = tokio::sync::mpsc::unbounded_channel();
        let manager = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<crate::runtime::WsServerManager>().clone()
        };
        {
            let mut connections = manager.write().await;
            connections.insert(
                "slow-client".to_string(),
                WsClientSink::new(frames, buffered.clone()),
            );
            // A second connection, so the outcome of the first can be read
            // back without a v8 scope. Its own counter, because the ceiling
            // is per client.
            connections.insert(
                "report".to_string(),
                WsClientSink::new(reports, Arc::new(AtomicUsize::new(0))),
            );
        }

        // A mebibyte at a time into a client that never reads.
        let send_until_refused = r#"
            (async () => {
                const frame = "x".repeat(1024 * 1024);
                let sent = 0;
                let refusal = "nothing was refused";
                for (let i = 0; i < 8; i++) {
                    const result = await Deno.core.ops.op_ws_server_send("slow-client", frame);
                    if (result.success) { sent++; continue; }
                    refusal = result.error;
                    break;
                }
                await Deno.core.ops.op_ws_server_send("report", sent + "|" + refusal);
            })()
        "#;
        let script = runtime
            .execute_script("<test>", send_until_refused)
            .unwrap();
        let settled = runtime.resolve(script);
        runtime
            .with_event_loop_promise(settled, PollEventLoopOptions::default())
            .await
            .unwrap();

        let outcome = match report.recv().await.expect("the outcome was reported") {
            WsServerMessage::Text(text) => text,
            other => panic!("the outcome arrived as {other:?}"),
        };
        let (sent, refusal) = outcome.split_once('|').expect("sent count and refusal");
        assert_eq!(
            sent, "2",
            "two mebibytes fit under the ceiling and the third must not: {refusal}"
        );
        assert!(
            refusal.contains("2097152") && refusal.contains("behind"),
            "the refusal has to say what happened and name the limit: {refusal}"
        );
        assert!(
            buffered.load(Ordering::Relaxed) <= 2 * 1024 * 1024,
            "the node is holding {} bytes for one client",
            buffered.load(Ordering::Relaxed)
        );

        // The bridge does its half: once a frame has been handed to the
        // socket, its bytes stop counting and the application may send again.
        // Without that the ceiling would be a one-way door.
        for _ in 0..2 {
            let frame = bridge.recv().await.expect("the bridge has the frames");
            buffered.fetch_sub(frame.queued_bytes(), Ordering::Relaxed);
        }

        let send_once_more = r#"
            (async () => {
                const result = await Deno.core.ops.op_ws_server_send(
                    "slow-client", "x".repeat(1024 * 1024));
                await Deno.core.ops.op_ws_server_send(
                    "report", result.success ? "sent" : ("still refused: " + result.error));
            })()
        "#;
        let script = runtime.execute_script("<test>", send_once_more).unwrap();
        let settled = runtime.resolve(script);
        runtime
            .with_event_loop_promise(settled, PollEventLoopOptions::default())
            .await
            .unwrap();

        match report
            .recv()
            .await
            .expect("the second outcome was reported")
        {
            WsServerMessage::Text(text) => assert_eq!(
                text, "sent",
                "a client that caught up must be sendable to again"
            ),
            other => panic!("the outcome arrived as {other:?}"),
        }
    }
}
