// This app's queue consumer.
//
// A leading underscore means the platform calls this file rather than a
// request routing to it: deis-build re-exports it from the entry module as
// `queue`, which is the name deis-runtime looks for. It is never a URL.
//
// The node leases a batch from deis-queue, hands it here, then acknowledges
// the messages this function did not mark for retry. Delivery is at-least-once
// and a redelivery is indistinguishable from the first attempt, so the work
// below is idempotent: writing the same key twice leaves the same value.
export default async function queue(batch: any, context: any): Promise<void> {
  for (const message of batch.messages) {
    let job: any;
    try {
      job = message.json();
    } catch (_) {
      // Malformed for good: retrying would burn every delivery attempt and
      // then dead-letter it anyway, so it is acknowledged and dropped here
      // with a line saying the work was thrown away.
      console.error(`queue ${batch.queue}: message ${message.id} is not JSON and was discarded`);
      continue;
    }

    try {
      await context.services.kv.put(`job:${job.key}`, job.value);
      console.log(`queue ${batch.queue}: handled ${job.key} on attempt ${message.attempts}`);
    } catch (e) {
      // Ask for this one back rather than the whole batch: the others may
      // well have succeeded.
      console.error(`queue ${batch.queue}: ${job.key} failed and will be retried: ${e}`);
      message.retry();
    }
  }
}
