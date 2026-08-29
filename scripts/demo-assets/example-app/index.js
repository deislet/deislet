export default async function handler(ctx) {
  const method = ctx?.request?.method ?? "GET";
  const url = ctx?.request?.url ?? "/";
  // Exercises the log path on every request: the line goes to the node's
  // stdout and, through the log shipper, to deis-telemetry, where
  // `LogService.Query` with app_id "demo" can find it again.
  console.log("demo app handled a request", { method, url });
  return new Response(
    JSON.stringify({
      app: "demo",
      message: "Hello from Runtime",
      method,
      url,
      time: new Date().toISOString(),
    }),
    { headers: { "content-type": "application/json" } },
  );
}
