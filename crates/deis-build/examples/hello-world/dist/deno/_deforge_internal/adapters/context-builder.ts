/**
 * Deno Context Builder
 */



export function createContextBuilder(kv?: Deno.Kv) {
  return function buildContext(request: Request, route: string = '/') {
    const url = new URL(request.url);

    return {
      request,
      env: Deno.env.toObject(),
      params: extractParams(url.pathname, route),
      path: url.pathname,
      method: request.method,
      query: Object.fromEntries(url.searchParams),
      headers: request.headers,
      cookies: parseCookies(request),
      services: {
        kv: undefined,
      },
      log: {
        debug: (...args: any[]) => console.debug(...args),
        info: (...args: any[]) => console.info(...args),
        warn: (...args: any[]) => console.warn(...args),
        error: (...args: any[]) => console.error(...args),
      },
      cache: {
        match: async (req: Request | string) => caches.open('v1').then(c => c.match(req)),
        put: async (req: Request | string, res: Response) => caches.open('v1').then(c => c.put(req, res)),
        delete: async (req: Request | string) => caches.open('v1').then(c => c.delete(req)),
      },
      waitUntil: (promise: Promise<any>) => {
        promise.catch(console.error);
      },
    };
  };
}

function extractParams(pathname: string, route: string): Record<string, string> {
  const params: Record<string, string> = {};
  const routeParts = route.split('/');
  const pathParts = pathname.split('/');

  routeParts.forEach((part, i) => {
    if (part.startsWith(':')) {
      const paramName = part.slice(1);
      params[paramName] = pathParts[i] || '';
    }
  });

  return params;
}

function parseCookies(request: Request): any {
  const cookies: Record<string, string> = {};
  const cookieHeader = request.headers.get('Cookie');

  if (cookieHeader) {
    cookieHeader.split(';').forEach(cookie => {
      const [name, ...rest] = cookie.split('=');
      cookies[name.trim()] = rest.join('=').trim();
    });
  }

  return {
    get(name: string) { return cookies[name]; },
    set(name: string, value: string) { cookies[name] = value; },
    delete(name: string) { delete cookies[name]; },
    getAll() { return { ...cookies }; }
  };
}
