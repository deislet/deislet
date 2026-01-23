/**
 * Tencent EdgeOne Main Entry
 * Auto-generated router
 */

import { createContextBuilder } from './adapters/context-builder';
import handler0 from './router';


export default {
  async fetch(request, event) {
    const buildContext = createContextBuilder(event);
    const url = new URL(request.url);
    const pathname = url.pathname;

  if (matchRoute(pathname, '/router')) {
    const context = buildContext(request, '/router');
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
