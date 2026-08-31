// A handler that spends a chosen number of milliseconds before answering, so
// that a cut-off can be observed rather than argued about.
//
// Busy rather than asleep on purpose. A cut-off is delivered by terminating
// the isolate, which takes effect the moment JavaScript runs again; a handler
// waiting on a timer is not running, so the difference between "cut off at the
// ceiling" and "answered late" would depend on when the event loop next got a
// turn. A loop is running the whole time, so the ceiling is what decides.
export default async function handler(context: any): Promise<Response> {
  const asked = new URL(context.request.url).searchParams.get("ms");
  const ms = Math.min(Math.max(Number(asked) || 0, 0), 60_000);

  const started = Date.now();
  while (Date.now() - started < ms) {
    // Nothing: the point is the time, not the work.
  }
  const spent = Date.now() - started;

  context.log.info(`slow handler finished after ${spent}ms`);
  return new Response(JSON.stringify({ requestedMs: ms, spentMs: spent }), {
    headers: { "content-type": "application/json; charset=utf-8" },
  });
}
