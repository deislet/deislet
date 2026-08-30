// The outbound `WebSocket` an application gets.
//
// The class is here and the connection is in `runtime/ws_client_ops.rs`
// because of where the decision has to be made, not because of what is
// convenient. `deno_websocket`'s own `WebSocket` is loaded in this isolate and
// binding it would be a single line, but its client dials with a connector of
// its own that never reaches `runtime/fetch_policy.rs` -- so an application
// would have a second way out of the isolate, past the one decision point the
// outbound policy is built around, straight to the control plane or the cloud
// metadata service. The ops behind this class put every destination through
// that policy and then connect to the address the policy judged. See the
// module comment in `runtime/ws_client_ops.rs`.
//
// What is here is the surface: the states, the events and the two methods an
// application expects, on top of deno_web's `EventTarget` so that
// `addEventListener("message", ...)` and `onmessage = ...` are the same
// mechanism they are everywhere else, and so that the events carry `data`,
// `code` and `reason` where the specification puts them.
//
// The read loop is one pending op at a time. `op_deis_ws_next_event` answers
// with whatever the socket says next, this turns it into a DOM event, and it
// asks again. A pending op does not keep the invocation from finishing --
// `runtime/isolate.rs` drives the event loop until the handler's promise
// settles, not until the loop is empty -- so a handler that leaves a socket
// open still returns its response at the usual moment, and the socket itself
// is closed by the deadline the ops hold it to.
//
// Which is why every state this class reports, and every event it dispatches,
// is gated on the invocation still being the one that opened the socket. The
// end-of-invocation sweep runs in `runtime/ws_client_ops.rs`, on the other
// side of the op boundary; an object here outlives it, on a heap the next
// request reuses. Left ungated, a socket the sweep had already closed went on
// answering `readyState: 1` while `send()` on it threw -- so the ordinary
// guard, `if (ws.readyState === WebSocket.OPEN)`, chose the branch that
// raises -- and the read that was pending when the sweep ran rejected inside
// the *next* invocation, where the catch below turned it into `error` and
// `close` events and ran the application's listeners on a request that had
// nothing to do with them. `op_deis_ws_invocation` is how the object learns
// what happened: it records the number when it is built and compares before
// it answers.

import { primordials } from "ext:core/mod.js";
import {
  op_deis_fetch_budget_ms,
  op_deis_ws_buffered_amount,
  op_deis_ws_close,
  op_deis_ws_connect,
  op_deis_ws_invocation,
  op_deis_ws_next_event,
  op_deis_ws_send_binary,
  op_deis_ws_send_text,
} from "ext:core/ops";

import * as domException from "ext:deno_web/01_dom_exception.js";
import * as event from "ext:deno_web/02_event.js";
import * as encoding from "ext:deno_web/08_text_encoding.js";
import * as file from "ext:deno_web/09_file.js";
import * as urlModule from "ext:deno_web/00_url.js";

const {
  ArrayBufferIsView,
  ArrayBufferPrototype,
  ArrayFrom,
  ObjectDefineProperties,
  ObjectDefineProperty,
  ObjectPrototypeIsPrototypeOf,
  SafeSet,
  SetPrototypeAdd,
  SetPrototypeHas,
  String,
  StringPrototypeEndsWith,
  SymbolToStringTag,
  TypeError,
  Uint8Array,
} = primordials;

// Bound at module load rather than read off the global object when a socket is
// opened: `js/runtime.js` replaces some of these names and an application may
// replace others, and a class that reached for the globals would be building
// its events out of whatever the application supplied.
const DOMException = domException.DOMException;
const NativeURL = urlModule.URL;
const TextEncoder = encoding.TextEncoder;
const Blob = file.Blob;
const utf8 = new TextEncoder();

const CONNECTING = 0;
const OPEN = 1;
const CLOSING = 2;
const CLOSED = 3;

// What `close()` sends when the application named no code. 1005 means "no
// status was given" and is the one code that may never travel on the wire, so
// "normal closure" is what an unqualified `close()` means.
const NORMAL_CLOSURE = 1000;

// The specification's limit on a close reason, in UTF-8 bytes: a close frame
// carries at most 125 bytes and two of them are the code.
const MAX_CLOSE_REASON_BYTES = 123;

// Read the destination the way the constructor is defined to read it.
function destinationOf(input) {
  let parsed;
  try {
    parsed = new NativeURL(input);
  } catch (e) {
    throw new DOMException(
      `Cannot open a WebSocket to ${input}: ${e.message}`,
      "SyntaxError",
    );
  }

  // `http:` and `https:` are accepted and rewritten, which is what the
  // specification says to do with them; anything else is not a WebSocket
  // destination at all.
  if (parsed.protocol === "http:") {
    parsed.protocol = "ws:";
  } else if (parsed.protocol === "https:") {
    parsed.protocol = "wss:";
  } else if (parsed.protocol !== "ws:" && parsed.protocol !== "wss:") {
    throw new DOMException(
      `Cannot open a WebSocket to ${parsed.href}: a WebSocket URL uses the ` +
        "ws: or wss: scheme",
      "SyntaxError",
    );
  }

  if (parsed.hash !== "" || StringPrototypeEndsWith(parsed.href, "#")) {
    throw new DOMException(
      `Cannot open a WebSocket to ${parsed.href}: a WebSocket URL may not ` +
        "carry a fragment",
      "SyntaxError",
    );
  }

  return parsed.href;
}

// The subprotocol list, as one string or an iterable of them.
function subprotocolsOf(protocols) {
  const list = typeof protocols === "string" ? [protocols] : ArrayFrom(protocols);
  const seen = new SafeSet();
  for (const protocol of list) {
    if (typeof protocol !== "string" || protocol === "") {
      throw new DOMException(
        "A WebSocket subprotocol must be a non-empty string",
        "SyntaxError",
      );
    }
    if (SetPrototypeHas(seen, protocol)) {
      throw new DOMException(
        `The WebSocket subprotocol ${protocol} is named more than once`,
        "SyntaxError",
      );
    }
    SetPrototypeAdd(seen, protocol);
  }
  return list;
}

class WebSocket extends event.EventTarget {
  #url = "";
  #readyState = CONNECTING;
  #binaryType = "blob";
  // The handle the ops know this socket by, or null before the handshake has
  // finished and after the socket has closed.
  #rid = null;
  #protocol = "";
  #extensions = "";
  // What `close()` asked for while the handshake was still in flight; acted on
  // as soon as there is a socket to close.
  #closeRequested = null;
  // The invocation this socket belongs to. Taken here rather than when the
  // handshake finishes: a handler that constructs a socket and answers without
  // awaiting it leaves the connect in flight, and the socket belongs to the
  // request that asked for it either way.
  #openedIn = op_deis_ws_invocation();

  constructor(url, protocols = []) {
    super();
    this.#url = destinationOf(url);
    this.#connect(subprotocolsOf(protocols));
  }

  // Whether the invocation that opened this socket has ended.
  //
  // When it has, the socket behind this object is already closed -- the sweep
  // in `runtime/ws_client_ops.rs` did that -- and nothing about it is this
  // object's to report or to act on any more.
  #reclaimed() {
    return op_deis_ws_invocation() !== this.#openedIn;
  }

  get url() {
    return this.#url;
  }

  get readyState() {
    // CLOSED, and not the state this object last set for itself: the socket is
    // gone, and an application guarding its `send()` with
    // `readyState === WebSocket.OPEN` has to be told the truth or it takes the
    // branch that throws.
    return this.#reclaimed() ? CLOSED : this.#readyState;
  }

  get protocol() {
    return this.#protocol;
  }

  get extensions() {
    return this.#extensions;
  }

  // Bytes handed to `send()` that are not on the wire yet. Answered by the op
  // that owns the queue, so it is the real number rather than a placeholder.
  get bufferedAmount() {
    // Nothing is waiting on a socket that is gone, and asking the op layer
    // about a handle it has reclaimed would log a warning for what is only a
    // property read.
    if (this.#rid === null || this.#reclaimed()) {
      return 0;
    }
    return op_deis_ws_buffered_amount(this.#rid);
  }

  get binaryType() {
    return this.#binaryType;
  }

  set binaryType(value) {
    if (value !== "blob" && value !== "arraybuffer") {
      throw new TypeError(
        `binaryType must be "blob" or "arraybuffer", not ${value}`,
      );
    }
    this.#binaryType = value;
  }

  send(data) {
    // Through the getter, so a reclaimed socket reads CLOSED and takes the
    // drop below rather than the throw above.
    const state = this.readyState;
    if (state === CONNECTING) {
      throw new DOMException(
        "The WebSocket is still connecting; wait for the open event",
        "InvalidStateError",
      );
    }
    // After `close()` a send is dropped rather than refused, which is what the
    // specification says to do with it.
    if (state !== OPEN || this.#rid === null) {
      return;
    }

    if (typeof data === "string") {
      op_deis_ws_send_text(this.#rid, data);
    } else if (ArrayBufferIsView(data)) {
      op_deis_ws_send_binary(
        this.#rid,
        new Uint8Array(data.buffer, data.byteOffset, data.byteLength),
      );
    } else if (ObjectPrototypeIsPrototypeOf(ArrayBufferPrototype, data)) {
      op_deis_ws_send_binary(this.#rid, new Uint8Array(data));
    } else if (file.isBlob(data)) {
      // A Blob's bytes are not in hand yet. The frame is queued when they
      // arrive, and a socket that closed in between simply drops it -- which
      // is what would have happened to a frame queued behind it anyway.
      data.arrayBuffer().then(
        (bytes) => {
          // Read back through the getter: these bytes can arrive after the
          // invocation that queued them has ended.
          if (this.readyState === OPEN && this.#rid !== null) {
            op_deis_ws_send_binary(this.#rid, new Uint8Array(bytes));
          }
        },
        (e) => this.#failed(`the Blob could not be read: ${e.message}`),
      );
    } else {
      op_deis_ws_send_text(this.#rid, String(data));
    }
  }

  close(code = undefined, reason = "") {
    if (
      code !== undefined &&
      code !== NORMAL_CLOSURE &&
      !(code >= 3000 && code <= 4999)
    ) {
      throw new DOMException(
        `${code} is not a close code an application may send: use 1000, or ` +
          "one of 3000-4999",
        "InvalidAccessError",
      );
    }
    if (utf8.encode(reason).byteLength > MAX_CLOSE_REASON_BYTES) {
      throw new DOMException(
        `The close reason is longer than ${MAX_CLOSE_REASON_BYTES} bytes`,
        "SyntaxError",
      );
    }

    // The getter again: closing a socket the platform has already reclaimed is
    // a no-op, not a call into the op layer with a dead handle.
    const state = this.readyState;
    if (state === CLOSING || state === CLOSED) {
      return;
    }
    if (state === CONNECTING) {
      this.#readyState = CLOSING;
      this.#closeRequested = { code: code ?? NORMAL_CLOSURE, reason };
      return;
    }

    this.#readyState = CLOSING;
    op_deis_ws_close(this.#rid, code ?? NORMAL_CLOSURE, reason);
  }

  async #connect(protocols) {
    let opened;
    try {
      // The same budget an outbound `fetch` is given, from the same op: how
      // much of this invocation is left, less the margin that leaves the
      // handler room to notice a failure and answer it.
      opened = await op_deis_ws_connect(
        this.#url,
        op_deis_fetch_budget_ms(),
        protocols,
      );
    } catch (e) {
      // A refusal by the outbound policy arrives here, and reaches the
      // application the way any failed connection does: an error event and
      // then a close. The reason is in the message, and the node has already
      // logged it with the destination.
      this.#failed(e.message);
      return;
    }

    // The handshake finished, but not necessarily inside the invocation that
    // started it. The op refuses to register a socket in that case and this is
    // the same judgement on this side of the boundary: no `open` event, no
    // read loop, nothing running in a request that did not ask for any of it.
    if (this.#reclaimed()) {
      this.#readyState = CLOSED;
      return;
    }

    this.#rid = opened.rid;
    this.#protocol = opened.protocol;
    this.#extensions = opened.extensions;

    if (this.#closeRequested !== null) {
      const { code, reason } = this.#closeRequested;
      this.#closeRequested = null;
      op_deis_ws_close(this.#rid, code, reason);
    } else {
      this.#readyState = OPEN;
      this.dispatchEvent(new event.Event("open"));
    }

    this.#read();
  }

  async #read() {
    while (this.#rid !== null) {
      let next;
      try {
        next = await op_deis_ws_next_event(this.#rid);
      } catch (e) {
        // The platform took the socket away: the invocation that opened it has
        // ended, or there is nothing behind the handle any more.
        this.#failed(e.message);
        return;
      }

      // The one await in this class that routinely settles in a different
      // invocation from the one that started it, and everything below
      // dispatches into the application. A socket the sweep reclaimed has no
      // events left that belong to anybody.
      if (this.#reclaimed()) {
        this.#rid = null;
        this.#readyState = CLOSED;
        return;
      }

      switch (next.kind) {
        case "text":
          this.dispatchEvent(
            new event.MessageEvent("message", {
              data: next.text,
              origin: this.#url,
            }),
          );
          break;
        case "binary":
          this.dispatchEvent(
            new event.MessageEvent("message", {
              data: this.#binaryType === "arraybuffer"
                ? next.data.buffer
                : new Blob([next.data]),
              origin: this.#url,
            }),
          );
          break;
        case "close":
          this.#rid = null;
          this.#readyState = CLOSED;
          this.dispatchEvent(
            new event.CloseEvent("close", {
              // 1006 is the code for a connection that ended without a closing
              // handshake, which is the one case that is not clean.
              wasClean: next.code !== 1006,
              code: next.code,
              reason: next.text ?? "",
            }),
          );
          return;
        default:
          this.#failed(next.text ?? "the WebSocket failed");
          return;
      }
    }
  }

  // The end of a socket that never opened, or that failed while open. Both are
  // reported as the specification reports them: an error event first, so a
  // handler that only listens for errors sees the reason, then a close so that
  // the state machine ends where it should -- unless the invocation that owned
  // the socket has ended, in which case the state machine still ends there and
  // nothing is dispatched. See below.
  #failed(message) {
    if (this.#readyState === CLOSED) {
      return;
    }
    const reclaimed = this.#reclaimed();
    this.#rid = null;
    this.#readyState = CLOSED;
    // Every way into this method can be reached after the invocation that
    // owned the socket has ended -- the pending read rejecting, the connect
    // rejecting, a Blob resolving late -- and all three of those rejections
    // say only that the platform took the socket back. Dispatching here would
    // run the application's `error` and `close` listeners inside whatever
    // invocation is running now, on its budget and beside its request. The
    // state machine still ends where it should; nobody is told, because there
    // is nobody left to tell.
    if (reclaimed) {
      return;
    }
    this.dispatchEvent(new event.ErrorEvent("error", { message }));
    this.dispatchEvent(
      new event.CloseEvent("close", {
        wasClean: false,
        code: 1006,
        reason: message,
      }),
    );
  }
}

// The four states, on the constructor and on the prototype, which is where
// `WebSocket.OPEN` and `ws.OPEN` respectively come from. Constants, so neither
// can be written over.
const STATES = {
  CONNECTING: { value: CONNECTING, enumerable: true },
  OPEN: { value: OPEN, enumerable: true },
  CLOSING: { value: CLOSING, enumerable: true },
  CLOSED: { value: CLOSED, enumerable: true },
};
ObjectDefineProperties(WebSocket, STATES);
ObjectDefineProperties(WebSocket.prototype, STATES);
ObjectDefineProperty(WebSocket.prototype, SymbolToStringTag, {
  value: "WebSocket",
  configurable: true,
});

// `onopen` and the rest. deno_web's own definition, so an assignment and an
// `addEventListener` for the same type behave together exactly as they do in a
// browser -- the handler property is one listener among the others, in the
// order it was set.
event.defineEventHandler(WebSocket.prototype, "open");
event.defineEventHandler(WebSocket.prototype, "message");
event.defineEventHandler(WebSocket.prototype, "error");
event.defineEventHandler(WebSocket.prototype, "close");

export { WebSocket };
