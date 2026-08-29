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
export default async function scheduled(event: any, context: any): Promise<void> {
  const at = new Date(event.scheduledTime).toISOString();
  await context.services.kv.put("cron:last-run", at);
  console.log(`scheduled run for ${event.cron} due at ${at}`);
}
