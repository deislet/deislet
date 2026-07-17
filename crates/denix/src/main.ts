// Denix 平台入口文件

import { ApplicationManager } from "./core/app_manager.ts";
import { HTTPGateway } from "./core/gateway.ts";

/**
 * 主函数
 */
async function main() {
  console.log("🚀 Starting Denix Platform...");
  console.log("=====================================");

  // 创建应用管理器
  const appManager = new ApplicationManager();

  // 创建 HTTP 网关
  const port = parseInt(Deno.env.get("PORT") || "8000");
  const gateway = new HTTPGateway(appManager, port);

  // 启动网关
  await gateway.start();

  console.log("=====================================");
  console.log("✅ Denix Platform is ready!");
  console.log(`📡 HTTP Server: http://localhost:${port}`);
  console.log(`📊 Health Check: http://localhost:${port}/health`);
  console.log(`📚 API Docs: http://localhost:${port}/`);
  console.log("=====================================");

  // 处理优雅退出
  const handleShutdown = async () => {
    console.log("\n🛑 Shutting down Denix Platform...");

    // 停止网关
    await gateway.stop();

    // 卸载所有应用
    const apps = appManager.listApps();
    for (const app of apps) {
      try {
        await appManager.unloadApp(app.id);
      } catch (error) {
        console.error(`Failed to unload app ${app.id}:`, error);
      }
    }

    console.log("✅ Shutdown complete");
    Deno.exit(0);
  };

  // 监听退出信号
  Deno.addSignalListener("SIGINT", handleShutdown);
  Deno.addSignalListener("SIGTERM", handleShutdown);
}

// 运行主函数
if (import.meta.main) {
  main().catch((error) => {
    console.error("❌ Fatal error:", error);
    Deno.exit(1);
  });
}
