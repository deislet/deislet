// A named binding, reached by the name this project's `.config.json` chose.
//
// `context.bindings.HOT` and `env.HOT` are the same object, and both are the
// platform's: a variable or a secret of that name is refused when the bundle
// is built. What each one reaches is a namespace of its own inside this
// application's tenant, so the same key written through HOT and through COLD
// is two values, and neither is the one `context.services.kv` holds.
//
// PUT stores the body under `?key=`, GET reads it back, and both answer with
// the binding they used so a caller can see which namespace it reached
// without trusting the name it asked for.
export default async function handler(context: any): Promise<Response> {
  const name = context.params.name;
  const binding = name === "default" ? context.services.kv : context.bindings[name];
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

  const url = new URL(context.request.url);
  const key = url.searchParams.get("key") ?? "k";

  if (context.request.method === "PUT" || context.request.method === "POST") {
    const value = await context.request.text();
    await binding.put(key, value);
    return new Response(
      JSON.stringify({ binding: name, key, stored: value }),
      { status: 201, headers: { "content-type": "application/json; charset=utf-8" } },
    );
  }

  const value = await binding.get(key);
  const page = await binding.list();
  return new Response(
    JSON.stringify({
      binding: name,
      // Which namespace the object itself says it speaks for. "" is the
      // unnamed one, which is what `context.services.kv` is.
      namespace: binding.binding,
      key,
      found: value !== null && value !== undefined,
      value: value ?? null,
      keys: page.keys.map((entry: any) => entry.name),
      sameObjectOnEnv: name === "default" ? null : context.env[name] === binding,
    }),
    { headers: { "content-type": "application/json; charset=utf-8" } },
  );
}
