// 系统管理应用 - 提供平台管理 API
// 通过 x-app-id: _system 访问

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const path = url.pathname;
    const method = request.method;

    // 健康检查
    if (path === "/health" && method === "GET") {
      return new Response(
        JSON.stringify({
          status: "healthy",
          message: "Denix Platform is running with x-app-id routing",
        }),
        {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }
      );
    }

    // 根路径 - 欢迎页面
    if (path === "/" && method === "GET") {
      return new Response(
        "<html><body><h1>Denix Platform</h1><p>Use x-app-id header to route requests</p></body></html>",
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
  },
};
