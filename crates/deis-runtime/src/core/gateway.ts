// HTTP gateway implementation
// Receives HTTP requests and routes them to the corresponding application

import { ApplicationManager } from "./app_manager.ts";
import type { AppConfig } from "./types.ts";

/**
 * HTTPGateway class
 * HTTP gateway and API server
 */
export class HTTPGateway {
  private appManager: ApplicationManager;
  private port: number;
  private server?: Deno.HttpServer;

  constructor(appManager: ApplicationManager, port = 8000) {
    this.appManager = appManager;
    this.port = port;
  }

  /**
   * Start the gateway server
   */
  async start(): Promise<void> {
    console.log(`[Gateway] Starting HTTP server on port ${this.port}...`);

    this.server = Deno.serve({
      port: this.port,
      handler: (req) => this.handleRequest(req),
    });

    console.log(`[Gateway] Server started on http://localhost:${this.port}`);
  }

  /**
   * Handle an HTTP request
   */
  private async handleRequest(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname;

    try {
      // Health check
      if (path === "/health") {
        return this.handleHealthCheck();
      }

      // Management API
      if (path.startsWith("/api/")) {
        return await this.handleAdminAPI(request);
      }

      // Application routes
      if (path.startsWith("/apps/")) {
        return await this.handleAppRequest(request);
      }

      // Root path
      if (path === "/") {
        return this.handleRoot();
      }

      return new Response("Not Found", { status: 404 });
    } catch (error) {
      console.error("[Gateway] Request error:", error);
      return new Response(
        `Internal Server Error: ${error instanceof Error ? error.message : String(error)}`,
        { status: 500 },
      );
    }
  }

  /**
   * Handle health check
   */
  private handleHealthCheck(): Response {
    const stats = this.appManager.getStats();
    return new Response(
      JSON.stringify({
        status: "healthy",
        timestamp: new Date().toISOString(),
        stats,
      }),
      {
        status: 200,
        headers: { "Content-Type": "application/json" },
      },
    );
  }

  /**
   * Handle the root path
   */
  private handleRoot(): Response {
    const stats = this.appManager.getStats();
    const apps = this.appManager.listApps();

    const html = `
<!DOCTYPE html>
<html>
<head>
  <title>Runtime - Deno Multi-tenant Platform</title>
  <style>
    body {
      font-family: system-ui, -apple-system, sans-serif;
      max-width: 1200px;
      margin: 0 auto;
      padding: 2rem;
      background: #f5f5f5;
    }
    h1 { color: #333; }
    .stats {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
      gap: 1rem;
      margin: 2rem 0;
    }
    .stat-card {
      background: white;
      padding: 1.5rem;
      border-radius: 8px;
      box-shadow: 0 2px 4px rgba(0,0,0,0.1);
    }
    .stat-value { font-size: 2rem; font-weight: bold; color: #4a90e2; }
    .stat-label { color: #666; margin-top: 0.5rem; }
    .apps {
      background: white;
      padding: 1.5rem;
      border-radius: 8px;
      box-shadow: 0 2px 4px rgba(0,0,0,0.1);
    }
    .app-item {
      padding: 1rem;
      border-bottom: 1px solid #eee;
      display: flex;
      justify-content: space-between;
      align-items: center;
    }
    .app-item:last-child { border-bottom: none; }
    .app-id { font-weight: bold; }
    .app-state {
      padding: 0.25rem 0.75rem;
      border-radius: 12px;
      font-size: 0.875rem;
    }
    .state-running { background: #d4edda; color: #155724; }
    .state-stopped { background: #f8d7da; color: #721c24; }
    .state-error { background: #f8d7da; color: #721c24; }
    .state-loading { background: #fff3cd; color: #856404; }
    .api-docs {
      background: white;
      padding: 1.5rem;
      border-radius: 8px;
      box-shadow: 0 2px 4px rgba(0,0,0,0.1);
      margin-top: 2rem;
    }
    code {
      background: #f4f4f4;
      padding: 0.25rem 0.5rem;
      border-radius: 4px;
      font-size: 0.9rem;
    }
    pre {
      background: #f4f4f4;
      padding: 1rem;
      border-radius: 4px;
      overflow-x: auto;
    }
  </style>
</head>
<body>
  <h1>🚀 Runtime Platform</h1>
  <p>A multi-tenant web platform built on Deno</p>

  <h2>Statistics</h2>
  <div class="stats">
    <div class="stat-card">
      <div class="stat-value">${stats.total}</div>
      <div class="stat-label">Total Apps</div>
    </div>
    <div class="stat-card">
      <div class="stat-value">${stats.running}</div>
      <div class="stat-label">Running</div>
    </div>
    <div class="stat-card">
      <div class="stat-value">${stats.stopped}</div>
      <div class="stat-label">Stopped</div>
    </div>
    <div class="stat-card">
      <div class="stat-value">${stats.error}</div>
      <div class="stat-label">Errors</div>
    </div>
  </div>

  <h2>Applications</h2>
  <div class="apps">
    ${
      apps.length === 0
        ? "<p>No applications deployed yet.</p>"
        : apps.map(
          (app) => `
      <div class="app-item">
        <div>
          <div class="app-id">${app.id}</div>
          <div style="color: #666; font-size: 0.875rem;">
            Created: ${app.createdAt.toLocaleString()}
          </div>
        </div>
        <span class="app-state state-${app.state}">${app.state}</span>
      </div>
    `,
        ).join("")
    }
  </div>

  <div class="api-docs">
    <h2>Quick Start</h2>
    <p>Deploy an application:</p>
    <pre><code>curl -X POST http://localhost:${this.port}/api/apps \\
  -H "Content-Type: application/json" \\
  -d '{
    "id": "hello",
    "code": "export default { fetch() { return new Response(\\\"Hello World\\\") } }"
  }'</code></pre>

    <p>Access your application:</p>
    <pre><code>curl http://localhost:${this.port}/apps/hello</code></pre>

    <p>List all applications:</p>
    <pre><code>curl http://localhost:${this.port}/api/apps</code></pre>
  </div>
</body>
</html>
    `;

    return new Response(html, {
      status: 200,
      headers: { "Content-Type": "text/html; charset=utf-8" },
    });
  }

  /**
   * Handle management API requests
   */
  private async handleAdminAPI(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname;
    const method = request.method;

    // POST /api/apps - create an app
    if (path === "/api/apps" && method === "POST") {
      return await this.handleCreateApp(request);
    }

    // GET /api/apps - list apps
    if (path === "/api/apps" && method === "GET") {
      return this.handleListApps();
    }

    // GET /api/apps/:id - get app info
    if (path.match(/^\/api\/apps\/[^/]+$/) && method === "GET") {
      const appId = path.split("/")[3];
      return this.handleGetApp(appId);
    }

    // PUT /api/apps/:id - update an app
    if (path.match(/^\/api\/apps\/[^/]+$/) && method === "PUT") {
      const appId = path.split("/")[3];
      return await this.handleUpdateApp(appId, request);
    }

    // DELETE /api/apps/:id - delete an app
    if (path.match(/^\/api\/apps\/[^/]+$/) && method === "DELETE") {
      const appId = path.split("/")[3];
      return await this.handleDeleteApp(appId);
    }

    return new Response("Not Found", { status: 404 });
  }

  /**
   * Create an application
   */
  private async handleCreateApp(request: Request): Promise<Response> {
    try {
      const body = await request.json();
      const { id, code, config } = body;

      if (!id || !code) {
        return new Response(
          JSON.stringify({ error: "Missing id or code" }),
          { status: 400, headers: { "Content-Type": "application/json" } },
        );
      }

      await this.appManager.loadApp(id, code, config || {});

      return new Response(
        JSON.stringify({ success: true, appId: id }),
        { status: 201, headers: { "Content-Type": "application/json" } },
      );
    } catch (error) {
      return new Response(
        JSON.stringify({
          error: error instanceof Error ? error.message : String(error),
        }),
        { status: 500, headers: { "Content-Type": "application/json" } },
      );
    }
  }

  /**
   * List applications
   */
  private handleListApps(): Response {
    const apps = this.appManager.listApps().map((app) => ({
      id: app.id,
      state: app.state,
      createdAt: app.createdAt,
      updatedAt: app.updatedAt,
      error: app.error,
    }));

    return new Response(JSON.stringify({ apps }), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    });
  }

  /**
   * Get application info
   */
  private handleGetApp(appId: string): Response {
    const app = this.appManager.getApp(appId);
    if (!app) {
      return new Response(
        JSON.stringify({ error: "App not found" }),
        { status: 404, headers: { "Content-Type": "application/json" } },
      );
    }

    return new Response(
      JSON.stringify({
        id: app.id,
        state: app.state,
        config: app.config,
        createdAt: app.createdAt,
        updatedAt: app.updatedAt,
        error: app.error,
      }),
      {
        status: 200,
        headers: { "Content-Type": "application/json" },
      },
    );
  }

  /**
   * Update an application
   */
  private async handleUpdateApp(
    appId: string,
    request: Request,
  ): Promise<Response> {
    try {
      const body = await request.json();
      const { code, config } = body;

      if (!code) {
        return new Response(
          JSON.stringify({ error: "Missing code" }),
          { status: 400, headers: { "Content-Type": "application/json" } },
        );
      }

      await this.appManager.updateApp(appId, code, config);

      return new Response(
        JSON.stringify({ success: true }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      );
    } catch (error) {
      return new Response(
        JSON.stringify({
          error: error instanceof Error ? error.message : String(error),
        }),
        { status: 500, headers: { "Content-Type": "application/json" } },
      );
    }
  }

  /**
   * Delete an application
   */
  private async handleDeleteApp(appId: string): Promise<Response> {
    try {
      await this.appManager.unloadApp(appId);

      return new Response(
        JSON.stringify({ success: true }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      );
    } catch (error) {
      return new Response(
        JSON.stringify({
          error: error instanceof Error ? error.message : String(error),
        }),
        { status: 500, headers: { "Content-Type": "application/json" } },
      );
    }
  }

  /**
   * Handle an application request
   */
  private async handleAppRequest(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const pathParts = url.pathname.split("/");

    // /apps/:appId/*
    if (pathParts.length < 3) {
      return new Response("Invalid app path", { status: 400 });
    }

    const appId = pathParts[2];
    if (!appId) {
      return new Response("Missing app ID", { status: 400 });
    }

    // Build the app-internal path
    const appPath = "/" + pathParts.slice(3).join("/");
    const appUrl = new URL(appPath + url.search, url.origin);

    // Create a new request object
    const appRequest = new Request(appUrl, {
      method: request.method,
      headers: request.headers,
      body: request.body,
    });

    // Execute the application
    return await this.appManager.executeApp(appId, appRequest);
  }

  /**
   * Stop the gateway server
   */
  async stop(): Promise<void> {
    if (this.server) {
      await this.server.shutdown();
      console.log("[Gateway] Server stopped");
    }
  }
}
