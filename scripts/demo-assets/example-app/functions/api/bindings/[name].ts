// A named binding, reached by the name this project's `.config.json` chose.
//
// The standard context has no second bindings table: every declared logical
// resource is reached by its stable name under `context.env`. A variable or a
// secret of the same name is refused when the bundle is built. Each KV binding
// reaches a namespace of its own inside this application's tenant.
//
// PUT stores the body under `?key=`, GET reads it back, and both answer with
// the binding they used so a caller can see which namespace it reached
// without trusting the name it asked for.
export default async function handler(context: any): Promise<Response> {
  const name = context.params.name;
  // `default` remains an HTTP route alias for older demo clients; it maps to
  // the explicit logical name KV and is not a context compatibility alias.
  const logicalName = name === "default" ? "KV" : name;
  const declared = ["KV", "HOT", "COLD"];
  const binding = declared.includes(logicalName) ? context.env[logicalName] : undefined;
  if (binding === undefined) {
    return new Response(
      JSON.stringify({
        binding: name,
        error: "this application declares no binding of that name",
        declared,
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
      // Which logical namespace the object itself says it speaks for.
      namespace: binding.binding,
      key,
      found: value !== null && value !== undefined,
      value: value ?? null,
      keys: page.keys.map((entry: any) => entry.name),
      sameObjectOnEnv: context.env[logicalName] === binding,
    }),
    { headers: { "content-type": "application/json; charset=utf-8" } },
  );
}
