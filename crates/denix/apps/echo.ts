// Echo 应用 - 返回请求信息

export default {
  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);

    // 收集请求信息
    const info: any = {
      method: request.method,
      url: request.url,
      path: url.pathname,
      headers: request.headers,
      timestamp: new Date().toISOString(),
    };

    // 如果有 body，读取它
    if (request.body) {
      try {
        info.body = await request.text();
      } catch {
        info.body = "[Unable to read body]";
      }
    }

    return new Response(JSON.stringify(info, null, 2), {
      status: 200,
      headers: {
        "content-type": "application/json",
      },
    });
  },
};
