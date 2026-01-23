//#region dist/cloudflare/adapters/kv-adapter.ts
/**
* Cloudflare KV Adapter
*/
function createKVAdapter(namespace) {
	return {
		async get(key) {
			return namespace.get(key);
		},
		async getJSON(key) {
			return namespace.get(key, "json");
		},
		async put(key, value, options) {
			await namespace.put(key, value, {
				expirationTtl: options?.expirationTtl,
				metadata: options?.metadata
			});
		},
		async putJSON(key, value, options) {
			await namespace.put(key, JSON.stringify(value), {
				expirationTtl: options?.expirationTtl,
				metadata: options?.metadata
			});
		},
		async delete(key) {
			await namespace.delete(key);
		},
		async list(options) {
			const result = await namespace.list(options);
			return {
				keys: result.keys,
				list_complete: result.list_complete,
				cursor: result.cursor
			};
		},
		async exists(key) {
			return await namespace.get(key) !== null;
		}
	};
}

//#endregion
//#region dist/cloudflare/adapters/context-builder.ts
/**
* Cloudflare Context Builder
*/
function createContextBuilder(env, ctx) {
	return function buildContext(request, route = "/") {
		const url = new URL(request.url);
		return {
			request,
			env: {},
			params: extractParams(url.pathname, route),
			path: url.pathname,
			method: request.method,
			query: Object.fromEntries(url.searchParams),
			headers: request.headers,
			cookies: parseCookies(request),
			services: { kv: env.KV ? createKVAdapter(env.KV) : void 0 },
			log: {
				debug: (...args) => console.debug(...args),
				info: (...args) => console.info(...args),
				warn: (...args) => console.warn(...args),
				error: (...args) => console.error(...args)
			},
			cache: {
				match: async (req) => caches.default.match(req),
				put: async (req, res) => caches.default.put(req, res),
				delete: async (req) => caches.default.delete(req)
			},
			waitUntil: (promise) => ctx.waitUntil(promise)
		};
	};
}
function extractParams(pathname, route) {
	const params = {};
	const routeParts = route.split("/");
	const pathParts = pathname.split("/");
	routeParts.forEach((part, i) => {
		if (part.startsWith(":")) {
			const paramName = part.slice(1);
			params[paramName] = pathParts[i] || "";
		}
	});
	return params;
}
function parseCookies(request) {
	const cookies = {};
	const cookieHeader = request.headers.get("Cookie");
	if (cookieHeader) cookieHeader.split(";").forEach((cookie) => {
		const [name, ...rest] = cookie.split("=");
		cookies[name.trim()] = rest.join("=").trim();
	});
	return {
		get(name) {
			return cookies[name];
		},
		set(name, value) {
			cookies[name] = value;
		},
		delete(name) {
			delete cookies[name];
		},
		getAll() {
			return { ...cookies };
		}
	};
}

//#endregion
//#region dist/cloudflare/router.ts
/**
* Cloudflare Workers Router
* Auto-generated entry point
*/
var router_default = { async fetch(request, env, ctx) {
	createContextBuilder(env, ctx);
	new URL(request.url);
	return new Response("Not Found", { status: 404 });
} };

//#endregion
export { router_default as default };
