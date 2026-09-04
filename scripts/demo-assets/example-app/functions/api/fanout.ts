// Make several outbound calls from one invocation, and report where the
// platform stopped them.
//
// The shape a subrequest ceiling exists for: one inbound request turning into
// many outbound ones. `?n=` says how many calls to make; `?url=` is the
// destination, which the node's egress policy has to allow. The calls are
// made one after another so that `completed` is exactly the number that got
// through before the refusal, rather than a race.
//
// A redirect is the interesting case and needs no `n` at all. `fetch` follows
// redirects, each hop is a separate connection leaving this node, and each hop
// therefore costs one of the invocation's subrequests. So a single `fetch` of
// a URL that redirects six times can exhaust a ceiling of six while this
// handler contains one call.
export default async function handler(context: any): Promise<Response> {
  const params = new URL(context.request.url).searchParams;
  const target = params.get("url");
  const wanted = Number(params.get("n") ?? "1");

  const answer = (body: Record<string, unknown>, status = 200): Response =>
    new Response(JSON.stringify(body), {
      status,
      headers: { "content-type": "application/json; charset=utf-8" },
    });

  if (!target) {
    return answer({ error: "pass the destination as ?url=<absolute http url>" }, 400);
  }
  if (!Number.isInteger(wanted) || wanted < 1 || wanted > 200) {
    return answer({ error: "?n= must be a whole number between 1 and 200" }, 400);
  }

  let completed = 0;
  let lastStatus: number | null = null;
  try {
    for (let i = 0; i < wanted; i++) {
      const response = await fetch(target);
      // The body is read and dropped: leaving it open would hold a connection
      // charged to this node for the rest of the invocation.
      await response.arrayBuffer();
      lastStatus = response.status;
      completed++;
    }
  } catch (e) {
    // A refusal reaches an application as a failed `fetch`, which is what the
    // specification says a refused call is. The message is reported verbatim
    // because it is the thing worth reading: it names the ceiling and says
    // that redirect hops count towards it.
    console.warn(`fanout stopped after ${completed} of ${wanted} call(s): ${e}`);
    return answer({ requested: wanted, completed, lastStatus, error: String(e) });
  }

  return answer({ requested: wanted, completed, lastStatus });
}
