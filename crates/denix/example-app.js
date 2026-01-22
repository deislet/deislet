// 示例 JavaScript 应用
// 展示 Denix 平台的基本能力

export default async function handler(context) {
  const request = context.request;
  const url = new URL(request.url);

  if (url.pathname === '/') {
    return new Response(JSON.stringify({
      message: "Hello from Denix!",
      timestamp: new Date().toISOString(),
      runtime: "Denix JavaScript Runtime",
      path: url.pathname
    }), {
      headers: {
        'Content-Type': 'application/json'
      }
    });
  }

  if (url.pathname === '/echo') {
    return new Response(JSON.stringify({
      method: request.method,
      url: request.url,
      headers: Object.fromEntries(request.headers)
    }), {
      headers: {
        'Content-Type': 'application/json'
      }
    });
  }

  return new Response('Not Found', { status: 404 });
}
