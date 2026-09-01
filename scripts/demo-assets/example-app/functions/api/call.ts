// Calling another application on this platform, through the service binding
// this project's `.config.json` declares as AUTH.
//
// It is an ordinary fetch as far as the code is concerned. What is different
// is that it never leaves this node: there is no address, so the outbound
// policy is not involved and cannot be widened by it, and a callee running
// somewhere else is refused in those words rather than reached over the
// network.
//
// Every refusal is caught and answered as text, because the four of them mean
// four different things -- the callee is not here, the callee will not take
// the call, it failed inside the callee, or this invocation ran out of time
// waiting -- and an author has to be able to tell which.
export default async function handler(context: any): Promise<Response> {
  const url = new URL(context.request.url);
  const name = url.searchParams.get("binding") ?? "AUTH";
  const path = url.searchParams.get("path") ?? "/verify";

  const binding = context.bindings[name];
  if (binding === undefined) {
    return new Response(
      JSON.stringify({
        binding: name,
        error: "this application declares no binding of that name",
        declared: Object.keys(context.bindings),
      }),
      { status: 404, headers: { "content-type": "application/json; charset=utf-8" } },
    );
  }

  try {
    const answer = await binding.fetch(path, {
      method: "POST",
      headers: {
        // Overwritten by the platform: what the callee reads is who really
        // called, not what this line says.
        "x-deis-caller": "a-caller-that-is-not-me",
        "x-demo-note": "sent by the caller",
      },
      body: "ticket-from-" + context.env.BUILD_LABEL,
    });
    return new Response(
      JSON.stringify({
        binding: name,
        // Which copy of which application this binding reaches. It follows
        // the caller's own environment unless the manifest names one.
        app: binding.app,
        environment: binding.environment,
        status: answer.status,
        body: await answer.text(),
      }),
      { headers: { "content-type": "application/json; charset=utf-8" } },
    );
  } catch (e: any) {
    return new Response(
      JSON.stringify({
        binding: name,
        app: binding.app,
        environment: binding.environment,
        refused: e.message,
      }),
      { status: 502, headers: { "content-type": "application/json; charset=utf-8" } },
    );
  }
}
