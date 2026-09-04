// A handler that runs its isolate out of heap on purpose.
//
// `deis_runtime_memory_cutoffs_total` and the `out-of-memory` verdict on an
// invocation are both written in one place: V8's near-heap-limit callback. It
// is the only thing on the node that knows an isolate died of its heap rather
// than of anything else - by the time the request path sees the failure, an
// out-of-memory kill and a handler that threw are the same terminated
// execution. A callback nothing ever fires leaves an operator reading "this
// never happens" off a counter nobody wrote, so the demo makes it happen.
//
// Dense arrays of numbers, not strings. `"x".repeat(n) + i` looks like an
// allocation and is not: V8 answers a concatenation with a rope that points
// at the pieces, so a hundred thousand of them cost almost nothing and the
// handler returns having filled no memory at all. An `Array(n).fill(i)` is
// backed by real elements in the heap this ceiling bounds. Each one is kept
// in `held`, because garbage a collector can reclaim is not pressure.
//
// The project declares `limits.memoryMb`, so the ceiling is a small fraction
// of the node's and this finishes long inside the invocation's time budget.
// Nothing below the loop is reached: the isolate is terminated inside it.
export default async function handler(context: any): Promise<Response> {
  const held: number[][] = [];

  // A caller may name this attempt, and the name is logged before the first
  // allocation. Every line one invocation produces carries the same
  // `trace_id`, and `deis trace` prints that id on the row it writes for the
  // invocation, so a caller that passes `?tag=` can find out which invocation
  // was its own and insist the trace row it judges be that one. Without it a
  // check watching for an out-of-heap ending is watching this environment for
  // one, and any caller who can reach this route produces one.
  const tag = new URL(context.request.url).searchParams.get("tag") ?? "";
  if (tag !== "") console.log("heap witness " + tag);

  console.warn("allocating until this isolate reaches its heap ceiling");
  for (let i = 0; i < 100_000; i++) {
    held.push(new Array(64 * 1024).fill(i));
  }

  return new Response(
    JSON.stringify({ blocks: held.length, unreachable: true }),
    { headers: { "content-type": "application/json; charset=utf-8" } },
  );
}
