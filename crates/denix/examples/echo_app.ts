// Echo 应用 - 返回请求信息

export default {
  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);

    // 收集请求信息
    const info = {
      method: request.method,
      url: request.url,
      path: url.pathname,
      query: Object.fromEntries(url.searchParams),
      headers: Object.fromEntries(request.headers),
      timestamp: new Date().toISOString(),
    };

    // 如果有 body，读取它
    if (request.body) {
      try {
        const body = await request.text();
        (info as any).body = body;
      } catch {
        (info as any).body = "[Unable to read body]";
      }
    }

    return new Response(JSON.stringify(info, null, 2), {
      status: 200,
      headers: {
        "Content-Type": "application/json",
      },
    });
  },
};
