// KV through `context.services.kv`, which reaches deis-store over gRPC.
//
// PUT stores the request body under the key, GET reads it back. Nothing here
// knows where deis-store lives: the address is handed to the node by the
// control plane with the deployment, and the node builds the client.

// Keys the platform's own machinery writes, which this endpoint must not.
//
// Nothing authenticates a caller here - the route is open to anything that
// can reach the node with this application's id - so any key it can write is
// a key an outsider can set to whatever they like. That is fine for the keys
// the demo's checks invent for themselves (each one is a name only that check
// knows), and not fine for `cron:*`: those hold the record the cron check
// reads as evidence that a tick really fired, and evidence that the endpoint
// under test can be asked to write is no evidence at all. The scheduled
// handler writes them through the KV binding, which does not come this way.
//
// Reading stays open: a check has to be able to see what the handler wrote.
const RESERVED_KEY_PREFIX = "cron:";

export default async function handler(context: any): Promise<Response> {
  const key = context.params.key;
  const method = context.request.method;

  if (
    (method === "PUT" || method === "POST") &&
    key.startsWith(RESERVED_KEY_PREFIX)
  ) {
    return new Response(
      JSON.stringify({
        key,
        stored: false,
        refused:
          `keys under "${RESERVED_KEY_PREFIX}" are written by the scheduled ` +
          `handler and by nothing that arrives over HTTP`,
      }),
      {
        status: 403,
        headers: { "content-type": "application/json; charset=utf-8" },
      },
    );
  }

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
