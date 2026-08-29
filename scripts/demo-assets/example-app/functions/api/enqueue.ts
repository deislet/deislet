// Produce one message on the `jobs` queue, which this app also consumes
// (see functions/_queue.ts). Delivery is at-least-once, so the consumer is
// written to be safe to run twice on the same message.
export default async function handler(context: any): Promise<Response> {
  const payload = await context.request.text();
  let job: any;
  try {
    job = JSON.parse(payload);
  } catch (_) {
    return new Response(JSON.stringify({ error: "body must be JSON: {\"key\":..,\"value\":..}" }), {
      status: 400,
      headers: { "content-type": "application/json; charset=utf-8" },
    });
  }

  if (typeof job.key !== "string" || typeof job.value !== "string") {
    return new Response(JSON.stringify({ error: "key and value must both be strings" }), {
      status: 400,
      headers: { "content-type": "application/json; charset=utf-8" },
    });
  }

  await context.services.queue.to("jobs").send(job);
  context.log.info(`queued job for ${job.key}`);

  // 202: the work has been accepted, not done. Whether it happened is
  // observable at /api/kv/job:<key>.
  return new Response(JSON.stringify({ queued: job.key }), {
    status: 202,
    headers: { "content-type": "application/json; charset=utf-8" },
  });
}
