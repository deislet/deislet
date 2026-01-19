// WebSocket Echo 应用示例
// 演示完整的 WebSocket 消息桥接功能

globalThis.default = {
  async fetch(request) {
    const url = new URL(request.url);

    // 检查是否是 WebSocket 升级请求
    if (request.headers.Upgrade === "websocket") {
      // 创建 WebSocket Pair
      const pair = new WebSocketPair();
      const server = pair.server;
      const client = pair.client;

      console.log("WebSocket connection requested, ws_id:", server.ws_id, "client:", client.ws_id);

      // 设置服务端事件处理器
      server.accept();
      console.log("WebSocket accepted");

      server.onopen = (event) => {
        console.log("WebSocket opened:", event);
        server.send("Welcome! Echo server ready. Send me a message!");
      };

      server.onmessage = (event) => {
        console.log("Received from client:", event.data);
        // Echo 回复
        const response = `Echo: ${event.data}`;
        server.send(response);
      };

      server.onerror = (event) => {
        console.error("WebSocket error:", event);
      };

      server.onclose = (event) => {
        console.log("WebSocket closed:", event.code, event.reason);
      };

      // 返回 101 响应，触发升级
      return new Response(null, {
        status: 101,
        webSocket: client,
      });
    }

    // 普通 HTTP 请求
    return new Response(
      `
      <html>
        <head><title>WebSocket Echo Test</title></head>
        <body>
          <h1>WebSocket Echo Test</h1>
          <p>Connect to <code>ws://localhost:8000/apps/ws-echo</code> to test</p>
          <h2>Test with wscat:</h2>
          <pre>npm install -g wscat
wscat -c ws://localhost:8000/apps/ws-echo</pre>
        </body>
      </html>
      `,
      {
        headers: { "Content-Type": "text/html" },
      }
    );
  },
};
