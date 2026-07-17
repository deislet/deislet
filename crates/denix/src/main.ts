// Denix platform entry point

import { ApplicationManager } from "./core/app_manager.ts";
import { HTTPGateway } from "./core/gateway.ts";

/**
 * Main function
 */
async function main() {
  console.log("🚀 Starting Denix Platform...");
  console.log("=====================================");

  // Create the application manager
  const appManager = new ApplicationManager();

  // Create the HTTP gateway
  const port = parseInt(Deno.env.get("PORT") || "8000");
  const gateway = new HTTPGateway(appManager, port);

  // Start the gateway
  await gateway.start();

  console.log("=====================================");
  console.log("✅ Denix Platform is ready!");
  console.log(`📡 HTTP Server: http://localhost:${port}`);
  console.log(`📊 Health Check: http://localhost:${port}/health`);
  console.log(`📚 API Docs: http://localhost:${port}/`);
  console.log("=====================================");

  // Handle graceful shutdown
  const handleShutdown = async () => {
    console.log("\n🛑 Shutting down Denix Platform...");

    // Stop the gateway
    await gateway.stop();

    // Unload all applications
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

  // Listen for exit signals
  Deno.addSignalListener("SIGINT", handleShutdown);
  Deno.addSignalListener("SIGTERM", handleShutdown);
}

// Run the main function
if (import.meta.main) {
  main().catch((error) => {
    console.error("❌ Fatal error:", error);
    Deno.exit(1);
  });
}
