// System management app - provides platform management APIs
// Accessible via x-deis-app-id: _system

export default async function handler(context) {
  const request = context.request;
  const url = new URL(request.url);
  const path = url.pathname;
  const method = request.method;

    // Health check
    if (path === "/health" && method === "GET") {
      return new Response(
        JSON.stringify({
          status: "healthy",
          message: "Runtime Platform is running with x-deis-app-id routing",
        }),
        {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }
      );
    }

    // Root path - welcome page
    if (path === "/" && method === "GET") {
      return new Response(
        "<html><body><h1>Runtime Platform</h1><p>Use x-deis-app-id header to route requests</p></body></html>",
        {
          status: 200,
          headers: { "Content-Type": "text/html" },
        }
      );
    }

    // 404 Not Found
    return new Response(
      JSON.stringify({
        error: "Not Found",
        path: path,
        method: method,
      }),
      { status: 404, headers: { "Content-Type": "application/json" } }
    );
}
