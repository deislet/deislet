/**
 * Cloudflare Workers Router
 * Auto-generated entry point
 */

import { createContextBuilder } from './adapters/context-builder';
import handler0 from '../src/index.ts';


export default {
  async fetch(request, env, ctx) {
    const buildContext = createContextBuilder(env, ctx);
    const url = new URL(request.url);

  if (matchRoute(url.pathname, '/')) {
    const context = buildContext(request, '/');
    return handler0(context);
  }

    // 404 Not Found
    return new Response('Not Found', { status: 404 });
  }
};

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
