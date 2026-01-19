// 简单的 TODO 应用

interface Todo {
  id: string;
  title: string;
  completed: boolean;
  createdAt: string;
}

export default {
  async fetch(request: Request, env: any): Promise<Response> {
    const url = new URL(request.url);
    const method = request.method;

    // GET /todos - 列出所有 TODOs
    if (url.pathname === "/todos" && method === "GET") {
      const todoKeys = await env.kv.list("todo:");
      const todos: Todo[] = [];

      for (const key of todoKeys) {
        const todoJson = await env.kv.get(key);
        if (todoJson) {
          try {
            todos.push(JSON.parse(todoJson));
          } catch (e) {
            console.error("Failed to parse todo:", e);
          }
        }
      }

      return new Response(JSON.stringify(todos, null, 2), {
        headers: { "content-type": "application/json" },
      });
    }

    // POST /todos - 创建新 TODO
    if (url.pathname === "/todos" && method === "POST") {
      const bodyText = await request.text();
      const body = JSON.parse(bodyText);

      const id = Math.random().toString(36).substring(7);
      const todo: Todo = {
        id,
        title: body.title,
        completed: false,
        createdAt: new Date().toISOString(),
      };

      await env.kv.set(`todo:${id}`, JSON.stringify(todo));

      return new Response(JSON.stringify(todo, null, 2), {
        status: 201,
        headers: { "content-type": "application/json" },
      });
    }

    // PUT /todos/:id - 更新 TODO
    const putMatch = url.pathname.match(/^\/todos\/([^\/]+)$/);
    if (putMatch && method === "PUT") {
      const id = putMatch[1];
      const existingTodo = await env.kv.get(`todo:${id}`);

      if (!existingTodo) {
        return new Response(JSON.stringify({ error: "Todo not found" }), {
          status: 404,
          headers: { "content-type": "application/json" },
        });
      }

      const bodyText = await request.text();
      const body = JSON.parse(bodyText);
      const todo: Todo = JSON.parse(existingTodo);

      todo.title = body.title ?? todo.title;
      todo.completed = body.completed ?? todo.completed;

      await env.kv.set(`todo:${id}`, JSON.stringify(todo));

      return new Response(JSON.stringify(todo, null, 2), {
        headers: { "content-type": "application/json" },
      });
    }

    // DELETE /todos/:id - 删除 TODO
    const deleteMatch = url.pathname.match(/^\/todos\/([^\/]+)$/);
    if (deleteMatch && method === "DELETE") {
      const id = deleteMatch[1];
      await env.kv.delete(`todo:${id}`);

      return new Response(JSON.stringify({ success: true }), {
        headers: { "content-type": "application/json" },
      });
    }

    return new Response("Not Found", { status: 404 });
  },
};
