// KV through `context.services.kv`, which reaches deis-store over gRPC.
//
// PUT stores the request body under the key, GET reads it back. Nothing here
// knows where deis-store lives: the address is handed to the node by the
// control plane with the deployment, and the node builds the client.
export default async function handler(context: any): Promise<Response> {
  const key = context.params.key;
  const method = context.request.method;

  if (method === "PUT" || method === "POST") {
    const value = await context.request.text();
    await context.services.kv.put(key, value);
    return new Response(JSON.stringify({ key, stored: value }), {
      status: 201,
      headers: { "content-type": "application/json; charset=utf-8" },
    });
  }

  const value = await context.services.kv.get(key);
  if (value === null || value === undefined) {
    return new Response(JSON.stringify({ key, found: false }), {
      status: 404,
      headers: { "content-type": "application/json; charset=utf-8" },
    });
  }

  return new Response(JSON.stringify({ key, found: true, value }), {
    headers: { "content-type": "application/json; charset=utf-8" },
  });
}
