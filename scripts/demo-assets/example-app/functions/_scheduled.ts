// This app's cron handler.
//
// Like _queue.ts, the leading underscore means the platform calls this file
// rather than a request routing to it: deis-build re-exports it as
// `scheduled`. The schedule itself lives in .config.json and travels in the
// bundle manifest; deis-control keeps it and, when an expression is due,
// picks one healthy node and triggers it there. Exactly one node runs a tick.
//
// A tick is fire-and-forget: it is not retried and a missed one is never made
// up. Anything that must not be lost belongs on a queue this handler fills.
//
// The signature is the canon's `scheduled(event, env, context)`
// (SPECIFICATION_EXT.md section 3.1), the same three-parameter shape as the
// queue consumer next door.
//
// What it records, and why it is not just "the last run was at":
//
//   * The record carries the tick's scheduled time *and* a counter that only
//     goes up. A reader can then tell a tick that fired while it was watching
//     from one an earlier run left in the store, which a single overwritten
//     timestamp cannot express: the same value is what "it ran a minute ago"
//     and "it has not run since the last demo" both look like.
//   * The same record is written a second time under `cron:tick:<counter>`.
//     That key does not exist until its tick fires, so a reader can wait for
//     one particular tick to appear instead of re-reading a key that is
//     already there and guessing whether the value moved.
//
// Both keys live under `cron:`, and functions/api/kv/[key].ts refuses a write
// to that prefix. This handler reaches the same store through the platform's
// binding rather than through that endpoint, so the refusal costs it nothing
// and leaves the record something no unauthenticated caller can produce.
export default async function scheduled(
  event: any,
  env: any,
  _context: any,
): Promise<void> {
  const at = new Date(event.scheduledTime).toISOString();

  // The counter continues from whatever the previous tick wrote. A missing
  // key - the state right after `demo.sh start` - and a value this handler
  // did not write both read as zero, so the first tick of a fresh store is
  // tick 1.
  let previousTick = 0;
  try {
    const previous = await env.KV.get("cron:last-run");
    if (previous) {
      const parsed = JSON.parse(previous);
      if (typeof parsed.tick === "number" && Number.isFinite(parsed.tick)) {
        previousTick = parsed.tick;
      }
    }
  } catch (_err) {
    previousTick = 0;
  }

  const tick = previousTick + 1;
  const record = JSON.stringify({ at, tick, cron: event.cron });

  // `cron:last-run` first and the per-tick key second, so that a reader which
  // waits for `cron:tick:<n>` to appear and then reads `cron:last-run` never
  // finds the second one older than the tick it just waited for.
  await env.KV.put("cron:last-run", record);
  await env.KV.put(`cron:tick:${tick}`, record);
  console.log(`scheduled run for ${event.cron} due at ${at} (tick ${tick})`);
}
