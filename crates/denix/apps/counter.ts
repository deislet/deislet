// 使用 KV 存储的计数器应用

export default {
  async fetch(request: Request, env: any): Promise<Response> {
    const url = new URL(request.url);

    // 获取当前计数
    if (url.pathname === "/" || url.pathname === "/count") {
      const count = await env.kv.get("count") || "0";
      return new Response(`Current count: ${count}`, {
        headers: { "content-type": "text/plain" },
      });
    }

    // 增加计数
    if (url.pathname === "/increment") {
      const countStr = await env.kv.get("count") || "0";
      const count = parseInt(countStr);
      const newCount = count + 1;
      await env.kv.set("count", newCount.toString());
      return new Response(`Count incremented to: ${newCount}`, {
        headers: { "content-type": "text/plain" },
      });
    }

    // 重置计数
    if (url.pathname === "/reset") {
      await env.kv.set("count", "0");
      return new Response("Count reset to 0", {
        headers: { "content-type": "text/plain" },
      });
    }

    return new Response(
      "Not Found\\n\\nAvailable routes:\\n- /count\\n- /increment\\n- /reset",
      {
        status: 404,
        headers: { "content-type": "text/plain" },
      }
    );
  },
};
