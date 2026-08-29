// Outbound HTTP from a handler, and the platform's egress policy around it.
//
// This handler relays: it calls whatever absolute URL the query string names
// and reports what came back. That is deliberately the shape nobody should
// ship, and it is exactly what makes the demo's egress check worth running.
// An application that can be told where to go is the situation an egress
// policy exists for, so the verification asks this handler for deis-store's
// own address and fails if it gets an answer from it.
//
// Three outcomes, and the difference between them is the whole point:
//
//   available:false  `fetch` is not in this isolate at all. Edge Canon makes
//                    it a basic requirement (SPECIFICATION_BASIC.md 8.1), so
//                    this is a platform that does not meet the canon - not a
//                    destination that was refused. Reported separately for
//                    that reason: a ReferenceError caught below would read
//                    exactly like a refusal, and a check that could not tell
//                    them apart would pass on a platform with no `fetch`.
//   reached:true     the call was made and the response is here.
//   reached:false    the call was not made, or failed. `error` says which.
export default async function handler(context: any): Promise<Response> {
  const available = typeof fetch === "function";

  const answer = (body: Record<string, unknown>, status = 200): Response =>
    new Response(JSON.stringify(body), {
      status,
      headers: { "content-type": "application/json; charset=utf-8" },
    });

  const target = new URL(context.request.url).searchParams.get("url");
  if (!target) {
    return answer(
      { available, error: "pass the destination as ?url=<absolute http url>" },
      400,
    );
  }

  if (!available) {
    context.log.error("fetch is not defined in this isolate");
    return answer({
      available,
      target,
      reached: false,
      error: "fetch is not defined in this isolate",
    });
  }

  try {
    const response = await fetch(target);
    // Bounded on purpose: this body is printed by the verification, and a
    // destination that answers with a megabyte should not fill a terminal.
    const body = (await response.text()).slice(0, 500);
    context.log.info(`fetched ${target} -> ${response.status}`);
    return answer({
      available,
      target,
      reached: true,
      status: response.status,
      body,
    });
  } catch (e) {
    // A refusal arrives here as an ordinary failed fetch, which is what the
    // canon says it should be: the platform declines, the application sees a
    // rejected promise and decides what to do about it.
    context.log.warn(`fetch to ${target} did not happen: ${e}`);
    return answer({
      available,
      target,
      reached: false,
      error: String(e),
    });
  }
}
