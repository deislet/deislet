//! What a queue handler is handed for a body that is not text.
//!
//! The queue carries bytes end to end now, so a message can reach a consumer
//! with no text form at all - the case `DeadLetterMessage.body` has always
//! been typed for, and which the rest of the path could not express while
//! `SendRequest.message_body` and `QueueMessage.body` were strings. A batch
//! reaches an isolate as JSON written into the source of the invocation
//! script, and JSON cannot spell such a byte, so the two kinds of body travel
//! differently; this is where that seam is checked from the side that matters,
//! the application's.
//!
//! Each message in the batch is one expectation, named by its id. The handler
//! calls `retry()` on the ones that did not hold, so a failure names itself in
//! `outcome.failed` instead of arriving as a bare assertion.

use std::collections::HashMap;

use deis_runtime::runtime::isolate::AppRuntime;
use deis_runtime::runtime::ServiceRouter;
use deis_runtime::runtime::{IsolateSpec, LeasedMessage, ServiceWiring};
use tokio::sync::oneshot;

const CHECKING_HANDLER: &str = r#"
export default function handler() { return new Response("unused"); }

function threw(fn) {
  try { fn(); return null; } catch (e) { return e; }
}

export async function queue(batch) {
  for (const m of batch.messages) {
    let ok = false;
    if (m.id === "text") {
      // A body that is text still arrives as text, and json() still parses
      // it: an application that was working before this change still works.
      ok = m.body === '{"job":"resize"}'
        && m.json().job === "resize"
        && new TextDecoder().decode(m.bytes()) === m.body;
    } else if (m.id === "binary") {
      // No text form at all, so `body` is null and bytes() is the only way
      // in. json() has to say that rather than fail on a parse.
      const refusal = threw(() => m.json());
      ok = m.body === null
        && Array.from(m.bytes()).join(",") === "0,128,255,254,65"
        && refusal instanceof TypeError
        && refusal.message.includes("not text");
    } else if (m.id === "looks-like-base64") {
      // Which form a body travelled in is decided on the Rust side and
      // carried with it. Reading it off the string would turn this
      // application's own text into bytes it never wrote.
      ok = m.body === "aGVsbG8=";
    }
    if (!ok) m.retry();
  }
}
"#;

fn leased(id: &str, body: Vec<u8>) -> LeasedMessage {
    LeasedMessage {
        id: id.to_string(),
        body,
        attempts: 1,
        lease_expires_at: 0,
        created_at: 1_700_000_000,
        lease_token: format!("lease-of-{id}"),
    }
}

#[tokio::test]
async fn a_queue_handler_reads_text_as_text_and_anything_else_as_bytes() {
    let mut runtime = AppRuntime::new(
        "queue_bytes_app.production".to_string(),
        CHECKING_HANDLER.to_string(),
        None,
        IsolateSpec::default(),
        &ServiceWiring::default(),
        // No pool behind this isolate, so a service call it made would be
        // refused as "not deployed on this node", which is what it is.
        ServiceRouter::detached(),
    )
    .expect("the module builds");
    runtime.init_module().await.expect("the module loads");

    let outcome = runtime
        .execute_queue_batch(
            "jobs".to_string(),
            vec![
                leased("text", br#"{"job":"resize"}"#.to_vec()),
                // Bytes no UTF-8 decoder accepts, so this message could not
                // have been enqueued at all before the queue carried bytes.
                leased("binary", vec![0x00, 0x80, 0xff, 0xfe, 0x41]),
                leased("looks-like-base64", b"aGVsbG8=".to_vec()),
            ],
        )
        .await
        .expect("the handler ran");

    assert!(
        outcome.failed.is_empty(),
        "the handler did not see what it was given for: {:?}",
        outcome.failed
    );
}

/// The producer half: every shape `queue.send` accepts becomes bytes.
///
/// This app has no queue bound, so each send is refused by the op - which is
/// the point. The refusal can only be reached once the argument has crossed
/// into Rust as a buffer, so an argument the `Queue` class failed to encode
/// would fail here with a marshalling error instead, naming the shape that
/// broke.
const SENDING_APP: &str = r#"
export default async function handler(ctx) {
  const q = ctx.services.queue;
  const out = [];
  for (const [name, value] of [
    ["string", "text"],
    ["object", { json: true }],
    ["arraybuffer", new Uint8Array([0, 128, 255]).buffer],
    ["view", new Uint8Array([0, 128, 255])],
  ]) {
    try {
      await q.send(value);
      out.push(name + ":sent");
    } catch (e) {
      out.push(name + ":" + e.message);
    }
  }
  return new Response(JSON.stringify(out));
}
"#;

#[tokio::test]
async fn every_shape_queue_send_accepts_reaches_the_op_as_bytes() {
    let mut runtime = AppRuntime::new(
        "queue_send_shapes_app.production".to_string(),
        SENDING_APP.to_string(),
        None,
        IsolateSpec::default(),
        &ServiceWiring::default(),
        // No pool behind this isolate, so a service call it made would be
        // refused as "not deployed on this node", which is what it is.
        ServiceRouter::detached(),
    )
    .expect("the module builds");
    runtime.init_module().await.expect("the module loads");

    let (tx, rx) = oneshot::channel();
    runtime
        .execute_fetch(
            "http://localhost/".to_string(),
            "GET".to_string(),
            HashMap::new(),
            None,
            tx,
        )
        .await
        .expect("the handler ran");
    let (_status, _headers, body) = rx.await.expect("an answer").expect("a response");
    let reported =
        String::from_utf8(body.into_bytes().unwrap_or_default()).expect("the handler answers JSON");

    for shape in ["string", "object", "arraybuffer", "view"] {
        let line = format!("{shape}:queue service is not available to this app");
        assert!(
            reported.contains(&line),
            "{shape} did not reach the op as bytes: {reported}"
        );
    }
}
