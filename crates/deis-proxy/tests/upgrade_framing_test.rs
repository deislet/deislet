//! The two halves of "an upgrade", against the real `deis-proxy` binary.
//!
//! A WebSocket and an over-large upload arrive at this proxy looking almost
//! the same: an HTTP/1.1 request carrying an `Upgrade` header and a body of no
//! declared length. What separates them is the node's answer. Until the node
//! has said `101 Switching Protocols` the bytes after the header are a request
//! body, bounded by `max_request_body_bytes`; after it they are frames of a
//! conversation that has no total size and must not be counted.
//!
//! Both directions run through a process, not a mock -- see
//! [`harness`] -- so what is exercised is the binary an operator runs.
//! `cargo test -p deis-proxy` builds that binary as part of this package, so
//! the hop is covered whenever the suite runs.

mod harness;

use harness::{BODY_CEILING, Backend, HANDSHAKE_ACCEPT, HANDSHAKE_KEY, Proxy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How much a test sends when it wants to be past [`BODY_CEILING`].
const OVER_THE_CEILING: usize = 4 * BODY_CEILING;

#[tokio::test]
async fn an_ordinary_request_reaches_the_backend_through_the_proxy() {
    // The floor everything else stands on: the binary starts, binds, picks an
    // upstream and forwards -- `Proxy::start` does not return until a request
    // has come back through it from the backend. Without this a failure below
    // could mean nothing more than "the proxy never came up".
    let backend = Backend::start().await;
    let proxy = Proxy::start(backend.addr).await;
    drop(proxy);
    backend.stop();
}

#[tokio::test]
async fn an_upgraded_connection_tunnels_more_bytes_than_the_body_ceiling() {
    // The half that must not be counted. The handshake is answered by the
    // backend and relayed by the proxy - `Sec-WebSocket-Accept` is the answer
    // to the key that was sent, so nothing on the path answered on the
    // backend's behalf - and the tunnel then carries four times the body
    // ceiling in each direction without being cut.
    let backend = Backend::start().await;
    let proxy = Proxy::start(backend.addr).await;

    let mut stream = TcpStream::connect(proxy.addr)
        .await
        .expect("connecting to the proxy");
    let head = harness::exchange_head(
        &mut stream,
        &format!(
            "GET /ws HTTP/1.1\r\n\
             Host: {}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {HANDSHAKE_KEY}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n",
            proxy.addr
        ),
    )
    .await;

    assert!(
        head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
        "the upgrade did not switch protocols; the proxy answered:\n{head}\nits log said:\n{}",
        proxy.log_tail()
    );
    assert!(
        head.to_ascii_lowercase()
            .contains(&format!("sec-websocket-accept: {HANDSHAKE_ACCEPT}").to_ascii_lowercase()),
        "Sec-WebSocket-Accept is not the answer to the key that was sent; something on the \
         path answered the handshake instead of relaying it:\n{head}"
    );

    let payload = vec![b'w'; OVER_THE_CEILING];
    stream
        .write_all(&payload)
        .await
        .expect("the tunnel accepts more than the body ceiling");
    let mut echoed = vec![0u8; payload.len()];
    stream.read_exact(&mut echoed).await.unwrap_or_else(|e| {
        panic!(
            "an upgraded connection was cut after {OVER_THE_CEILING} bytes, which is what \
             counting its frames against max_request_body_bytes looks like: {e}; the proxy's \
             log said:\n{}",
            proxy.log_tail()
        )
    });
    assert_eq!(echoed, payload, "the tunnel did not carry the bytes intact");

    drop(proxy);
    backend.stop();
}

#[tokio::test]
async fn asking_for_an_upgrade_does_not_exempt_an_upload_from_the_body_ceiling() {
    // The half that must be counted, and the reason this file exists.
    //
    // `Upgrade` on a request is not an upgraded connection - pingora's
    // `is_upgrade_req` is satisfied by HTTP/1.1 plus the header alone, with no
    // `Connection`, no key and no answer from the node. Pingora reads this
    // body as chunked until the backend answers 101, and this backend never
    // does for `/upload`. The declared-length check in `request_filter`
    // cannot catch it either, because a chunked body declares no length. So
    // the only thing between an unbounded upload and the node is the
    // streaming count, and it only applies if the exemption asks whether the
    // connection *was* upgraded rather than whether the client asked for one.
    let backend = Backend::start().await;
    let proxy = Proxy::start(backend.addr).await;

    let mut stream = TcpStream::connect(proxy.addr)
        .await
        .expect("connecting to the proxy");
    let request = format!(
        "POST /upload HTTP/1.1\r\n\
         Host: {}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Transfer-Encoding: chunked\r\n\
         \r\n",
        proxy.addr
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("the request head is sent");

    // Written in pieces so the refusal can land on the chunk that crosses the
    // line rather than on the whole upload; the writes are allowed to fail
    // once it has, which is the proxy stopping the upload mid-flight.
    let piece = vec![b'u'; BODY_CEILING / 2];
    for _ in 0..(OVER_THE_CEILING / piece.len() + 1) {
        if stream.write_all(&harness::chunk(&piece)).await.is_err() {
            break;
        }
    }
    let _ = stream.write_all(b"0\r\n\r\n").await;

    // Tolerant of a reset, for the reason `read_until_closed` documents: the
    // proxy stops reading the upload the moment it refuses it, and closing a
    // socket that still holds the rest of that upload resets the connection,
    // which throws away the `413` this side had already been sent. That makes
    // the client's view of a refused upload genuinely two-valued, so the
    // assertions that must hold every time are on the two witnesses no reset
    // can touch: what the node was made to swallow, and what the proxy wrote
    // down about the decision.
    let answer = harness::read_until_closed(&mut stream).await;

    assert_eq!(
        backend.completed_uploads(),
        0,
        "the node received a complete {OVER_THE_CEILING} byte upload through a proxy whose \
         ceiling is {BODY_CEILING}, so naming an upgrade that never completed exempted the body \
         from the count; the proxy's log said:\n{}",
        proxy.log_tail()
    );
    assert!(
        proxy
            .log_text()
            .contains("a request body went past the ceiling while streaming"),
        "the proxy never recorded refusing the upload, so nothing counted the body it was \
         streaming; it answered {answer:?} and its log said:\n{}",
        proxy.log_tail()
    );
    assert!(
        answer.is_empty() || answer.starts_with("HTTP/1.1 413"),
        "the proxy answered something other than the refusal:\n{answer}\nits log said:\n{}",
        proxy.log_tail()
    );
    assert!(
        !answer.contains("uploaded"),
        "the backend saw the whole upload, so the proxy carried it across the edge-to-node hop \
         before refusing it:\n{answer}"
    );

    drop(proxy);
    backend.stop();
}
