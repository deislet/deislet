export default async function handler(ctx) {
  const method = ctx?.request?.method ?? "GET";
  const url = ctx?.request?.url ?? "/";
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
