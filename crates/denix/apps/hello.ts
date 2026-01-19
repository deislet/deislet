// 最简单的 Hello World 应用

export default {
  fetch(_request: Request): Response {
    return new Response("Hello from Denix (Rust + TypeScript)!", {
      status: 200,
      headers: {
        "content-type": "text/plain",
      },
    });
  },
};
