// System management app - provides platform management APIs
// Accessible via x-deis-app-id: _system

// What the node stamps on a request for this application to say how it is
// doing, and the one state that means it is running what it was assigned.
// Spelled in src/health.rs as READINESS_HEADER, SERVING_APPS_HEADER and
// READINESS_READY; the spellings are held together by
// health::tests::the_system_application_answers_the_probe_from_the_header,
// which runs this file.
//
// These cannot arrive from a client: dispatcher/handlers.rs drops the whole
// x-deis- namespace off an incoming request before an isolate sees it, and
// puts back only what the node itself decided. So reading them here is reading
// the node, not the caller.
const READINESS_HEADER = "x-deis-node-readiness";
const SERVING_APPS_HEADER = "x-deis-node-apps";
const READINESS_READY = "ready";
const READINESS_DEGRADED = "degraded";
const READINESS_STARTING = "starting";
const READINESS_UNKNOWN = "unknown";

export default async function handler(context) {
  const request = context.request;
  const url = new URL(request.url);
  const path = url.pathname;
  const method = request.method;

    // Health check.
    //
    // This is the edge's probe: deis-proxy asks GET /health with
    // x-deis-app-id: _system, evicts an upstream that does not answer 200, and
    // then reads the readiness this answer states to decide what to do with a
    // node that is up but not running what it was told to. This handler used
    // to answer 200 from a literal that consulted nothing, so a node that
    // could not reach its control plane and had nothing loaded passed the
    // probe for ever while answering 500 to every real request.
    //
    // The two halves of the answer say different things:
    //
    //   status  -- can this node serve anything at all? The node's own veto,
    //              understood by every probe including ones that know nothing
    //              about this platform.
    //   header  -- is this node running what it was assigned? The edge's
    //              policy decides what that is worth; a degraded fleet is
    //              still a serving fleet, so this must not be folded into the
    //              status.
    //
    // 200 for a ready node, and for a degraded node that is still serving at
    // least one application - it has stale versions rather than none, and
    // every node in a fleet goes degraded together when a control plane goes
    // down, so answering 503 there would turn an outage that costs nothing
    // into a total one. 503 for a starting node, for a degraded node with
    // nothing loaded, and for a node whose readiness never reached this
    // handler: those are the nodes that fail every request they are given.
    if (path === "/health" && method === "GET") {
      const readiness = request.headers.get(READINESS_HEADER) || READINESS_UNKNOWN;
      const reported = request.headers.get(SERVING_APPS_HEADER);
      // A count this handler cannot parse is treated as no applications, which
      // is the reading that keeps traffic away from a node nothing can be said
      // about.
      const apps = Number.isInteger(Number(reported)) ? Number(reported) : 0;

      const serving =
        readiness === READINESS_READY ||
        (readiness === READINESS_DEGRADED && apps > 0);

      let message;
      if (readiness === READINESS_READY) {
        message =
          "This node is running what the control plane assigned it: " +
          apps +
          " application(s)";
      } else if (readiness === READINESS_DEGRADED && apps > 0) {
        message =
          "This node cannot reach its control plane and is serving the " +
          apps +
          " application(s) it recovered from its own disk, which may be older " +
          "than the versions it is assigned";
      } else if (readiness === READINESS_DEGRADED) {
        message =
          "This node cannot reach its control plane and has no application " +
          "loaded, so every request it is given fails";
      } else if (readiness === READINESS_UNKNOWN) {
        message =
          "This node did not state its readiness on this request, so nothing " +
          "here can vouch for it";
      } else if (readiness === READINESS_STARTING) {
        message =
          "This node has not finished loading its assignment list, so it has " +
          "nothing to serve yet";
      } else {
        // Only reachable if the node starts stamping a state this handler has
        // not been taught. Naming it is the whole of what can honestly be
        // said, and refusing traffic is the safe half of the guess.
        message =
          "This node reports a readiness this handler does not recognise: " +
          readiness;
      }

      return new Response(
        JSON.stringify({
          status: serving ? "healthy" : "unhealthy",
          readiness: readiness,
          apps: apps,
          message: message,
        }),
        {
          status: serving ? 200 : 503,
          headers: {
            "Content-Type": "application/json",
            // The edge reads its rotation decision off this header, not off
            // the body: pingora hands its health-check validator the response
            // head and never the body.
            [READINESS_HEADER]: readiness,
            // A probe answer describes this instant. A cache between the edge
            // and the node that replayed a 200 would put an unhealthy node
            // back into rotation for as long as it held the entry.
            "Cache-Control": "no-store",
          },
        }
      );
    }

    // Root path - welcome page
    if (path === "/" && method === "GET") {
      return new Response(
        "<html><body><h1>Runtime Platform</h1><p>Use x-deis-app-id header to route requests</p></body></html>",
        {
          status: 200,
          headers: { "Content-Type": "text/html" },
        }
      );
    }

    // 404 Not Found
    return new Response(
      JSON.stringify({
        error: "Not Found",
        path: path,
        method: method,
      }),
      { status: 404, headers: { "Content-Type": "application/json" } }
    );
}
