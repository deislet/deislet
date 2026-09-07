// `env.ASSETS.fetch()`: a handler asking the asset layer for one of its own
// version's files.
//
// The HTTP entry consults the bundle before it starts an isolate, so a request
// whose path names a file never reaches a handler at all. This is the way back
// in - for the handler that ran anyway and wants the asset layer's answer for
// a path it chose: a route that authenticates a visitor and then serves the
// file behind it, a catch-all that answers `/` from `index.html`.
//
// The binding is also the one name on `context.env` an application cannot
// take: a project that writes `ASSETS` into its own environment is refused
// when it is built, so this project does not have one and could not have one.
// The report below therefore says what `env.ASSETS` actually is on a project
// that was accepted - an object with a `fetch` - which is the second half of
// the same claim: the platform owns the name, and the platform's object is
// what a handler gets.
export default async function handler(context: any): Promise<Response> {
  const answer = (body: Record<string, unknown>, status = 200): Response =>
    new Response(JSON.stringify(body), {
      status,
      headers: { "content-type": "application/json; charset=utf-8" },
    });

  const binding = context.env.ASSETS;
  const kind = binding === null ? "null" : typeof binding;
  const usable = kind === "object" && typeof binding.fetch === "function";
  const report = {
    // What the name holds. "object" is the binding; "string" would mean an
    // ordinary value reached `context.env` under this name and won, which is
    // the outcome the build refusal and the install order between them exist
    // to make impossible.
    bindingKind: kind,
    bindingIsCallable: usable,
    declaredValueVisible: kind === "string" ? binding : null,
  };

  if (!usable) {
    console.error("env.ASSETS is not the platform's binding");
    return answer(
      { ...report, error: "env.ASSETS is not an object with a fetch()" },
      500,
    );
  }

  // Chosen by the caller so the same handler can be pointed at a file the
  // bundle has and at a path it does not: one answers with the file's own
  // bytes, the other with the site's not-found behaviour, and both come back
  // as an ordinary Response.
  const target = new URL(context.request.url).searchParams.get("path")
    || "/css/app.css";

  const response = await binding.fetch(target);
  const body = await response.text();
  console.log(`env.ASSETS.fetch(${target}) -> ${response.status}`);

  return answer({
    ...report,
    target,
    status: response.status,
    contentType: response.headers.get("content-type"),
    bytes: body.length,
    body,
  });
}
