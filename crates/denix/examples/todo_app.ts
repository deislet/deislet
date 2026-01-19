// 简单的 TODO 应用

export default {
  async fetch(request: Request, env: any): Promise<Response> {
    const url = new URL(request.url);
    const method = request.method;

    // GET /todos - 列出所有 TODOs
    if (url.pathname === "/todos" && method === "GET") {
      const todoKeys = await env.kv.list("todo:");
      const todos = [];

      for (const key of todoKeys) {
        const todo = await env.kv.get(key);
        if (todo) {
          todos.push(JSON.parse(todo));
        }
      }

      return new Response(JSON.stringify(todos, null, 2), {
        headers: { "Content-Type": "application/json" },
      });
    }

    // POST /todos - 创建新 TODO
    if (url.pathname === "/todos" && method === "POST") {
      const body = await request.json();
      const id = crypto.randomUUID();
      const todo = {
        id,
        title: body.title,
        completed: false,
        createdAt: new Date().toISOString(),
      };

      await env.kv.set(`todo:${id}`, JSON.stringify(todo));

      return new Response(JSON.stringify(todo, null, 2), {
        status: 201,
        headers: { "Content-Type": "application/json" },
      });
    }

    // PUT /todos/:id - 更新 TODO
    if (url.pathname.match(/^\/todos\/[^/]+$/) && method === "PUT") {
      const id = url.pathname.split("/")[2];
      const existingTodo = await env.kv.get(`todo:${id}`);

      if (!existingTodo) {
        return new Response(JSON.stringify({ error: "Todo not found" }), {
          status: 404,
          headers: { "Content-Type": "application/json" },
        });
      }

      const body = await request.json();
      const todo = JSON.parse(existingTodo);
      todo.title = body.title ?? todo.title;
      todo.completed = body.completed ?? todo.completed;
      todo.updatedAt = new Date().toISOString();

      await env.kv.set(`todo:${id}`, JSON.stringify(todo));

      return new Response(JSON.stringify(todo, null, 2), {
        headers: { "Content-Type": "application/json" },
      });
    }

    // DELETE /todos/:id - 删除 TODO
    if (url.pathname.match(/^\/todos\/[^/]+$/) && method === "DELETE") {
      const id = url.pathname.split("/")[2];
      await env.kv.delete(`todo:${id}`);

      return new Response(JSON.stringify({ success: true }), {
        headers: { "Content-Type": "application/json" },
      });
    }

    return new Response("Not Found", { status: 404 });
  },
};
