// Produce a whole array of messages on the `jobs` queue in one call.
//
// The difference from functions/api/enqueue.ts is not convenience. One
// `sendBatch` is one op dispatch against this application's op-rate ceiling
// and one round trip to the queue, where the same array sent with `send` in a
// loop is one of each per message; and deis-queue stores a batch in a single
// transaction, so a rejected `sendBatch` has enqueued nothing at all and the
// same array can simply be sent again.
//
// Each entry is an object with a `body`, which is what `sendBatch` requires:
// `send(x)` takes the message itself, so an array of bare values and an array
// of `{ body }` wrappers could not both be read without one silently meaning
// the other.
export default async function handler(context: any): Promise<Response> {
  const answer = (body: Record<string, unknown>, status = 200): Response =>
    new Response(JSON.stringify(body), {
      status,
      headers: { "content-type": "application/json; charset=utf-8" },
    });

  let jobs: any;
  try {
    jobs = JSON.parse(await context.request.text());
  } catch (_) {
    return answer({ error: "body must be JSON: [{\"key\":..,\"value\":..}, ...]" }, 400);
  }

  if (!Array.isArray(jobs) || jobs.length === 0) {
    return answer({ error: "body must be a non-empty JSON array of jobs" }, 400);
  }
  for (const job of jobs) {
    if (typeof job?.key !== "string" || typeof job?.value !== "string") {
      return answer({ error: "every job needs a string key and a string value" }, 400);
    }
  }

  try {
    await context.env.jobs.sendBatch(jobs.map((job: any) => ({ body: job })));
  } catch (e) {
    // Worth reporting rather than swallowing: the batch is all-or-nothing, so
    // this means none of these jobs is queued and the caller may retry the
    // identical array.
    console.error(`sendBatch of ${jobs.length} job(s) failed: ${e}`);
    return answer({ queued: 0, error: String(e) }, 502);
  }

  console.log(`queued ${jobs.length} job(s) in one batch`);

  // 202 for the same reason /api/enqueue answers 202: the work is accepted,
  // not done. Whether it happened is observable at /api/kv/job:<key>.
  return answer({ queued: jobs.length, keys: jobs.map((job: any) => job.key) }, 202);
}
