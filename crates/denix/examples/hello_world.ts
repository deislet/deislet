// 最简单的 Hello World 应用

export default {
  async fetch(_request: Request): Promise<Response> {
    return new Response("Hello from Denix!", {
      status: 200,
      headers: {
        "Content-Type": "text/plain",
      },
    });
  },
};
