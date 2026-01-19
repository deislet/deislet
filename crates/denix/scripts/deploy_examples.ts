// 部署示例应用的脚本

const DENIX_URL = Deno.env.get("DENIX_URL") || "http://localhost:8000";

interface AppDef {
  id: string;
  file: string;
  description: string;
}

const apps: AppDef[] = [
  {
    id: "hello",
    file: "./examples/hello_world.ts",
    description: "Simple Hello World app",
  },
  {
    id: "counter",
    file: "./examples/counter_app.ts",
    description: "Counter app using KV storage",
  },
  {
    id: "echo",
    file: "./examples/echo_app.ts",
    description: "Echo app that returns request info",
  },
  {
    id: "todos",
    file: "./examples/todo_app.ts",
    description: "Simple TODO app with CRUD operations",
  },
];

async function deployApp(app: AppDef) {
  console.log(`\n📦 Deploying ${app.id}...`);
  console.log(`   ${app.description}`);

  try {
    // 读取应用代码
    const code = await Deno.readTextFile(app.file);

    // 发送部署请求
    const response = await fetch(`${DENIX_URL}/api/apps`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        id: app.id,
        code,
        config: {
          resources: ["kv"],
        },
      }),
    });

    if (response.ok) {
      console.log(`   ✅ Deployed successfully`);
      console.log(`   🌐 Access at: ${DENIX_URL}/apps/${app.id}`);
    } else {
      const error = await response.text();
      console.log(`   ❌ Failed to deploy: ${error}`);
    }
  } catch (error) {
    console.log(`   ❌ Error: ${error}`);
  }
}

async function main() {
  console.log("🚀 Deploying example apps to Denix...");
  console.log(`📡 Target: ${DENIX_URL}`);

  // 检查服务器是否运行
  try {
    const healthCheck = await fetch(`${DENIX_URL}/health`);
    if (!healthCheck.ok) {
      throw new Error("Server not healthy");
    }
  } catch {
    console.log("\n❌ Error: Denix server is not running!");
    console.log("   Please start it first: deno task dev");
    Deno.exit(1);
  }

  // 部署所有应用
  for (const app of apps) {
    await deployApp(app);
  }

  console.log("\n✅ All apps deployed!");
  console.log("\n📚 Try these commands:");
  console.log(`   curl ${DENIX_URL}/apps/hello`);
  console.log(`   curl ${DENIX_URL}/apps/counter/increment`);
  console.log(`   curl ${DENIX_URL}/apps/echo -d "test"`);
  console.log(`   curl ${DENIX_URL}/apps/todos/todos`);
}

if (import.meta.main) {
  main();
}
