/**
 * Deno Deploy Main Entry
 * Auto-generated router
 */

import handler0 from './index.ts';
import { createContextBuilder } from './adapters/context-builder.ts';


const kv = undefined; // await Deno.openKv();
const buildContext = createContextBuilder(kv);

Deno.serve(async (request: Request): Promise<Response> => {
  const url = new URL(request.url);

  if (matchRoute(url.pathname, '/')) {
    const context = buildContext(request, '/');
    return handler0(context);
  }

  // 404 Not Found
  return new Response('Not Found', { status: 404 });
});

function matchRoute(pathname: string, route: string): boolean {
  // Exact match
  if (pathname === route) return true;

  // Dynamic parameter match
  const routeParts = route.split('/');
  const pathParts = pathname.split('/');

  if (routeParts.length !== pathParts.length) return false;

  return routeParts.every((part, i) => {
    if (part.startsWith(':')) return true;
    return part === pathParts[i];
  });
}

console.log('🚀 Deno Deploy server running...');
