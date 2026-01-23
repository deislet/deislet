//#region dist/tencent/adapters/context-builder.ts
/**
* EdgeOne Context Builder
*/
function createContextBuilder(event) {
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
			services: { kv: void 0 },
			log: console,
			waitUntil: (promise) => event.waitUntil(promise)
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
//#region dist/tencent/router.ts
/**
* Tencent EdgeOne Main Entry
* Auto-generated router
*/
var router_default = { async fetch(request, event) {
	const buildContext = createContextBuilder(event);
	const pathname = new URL(request.url).pathname;
	if (matchRoute(pathname, "/router")) return router_default(buildContext(request, "/router"));
	return new Response("Not Found", { status: 404 });
} };
function matchRoute(pathname, route) {
	if (pathname === route) return true;
	const routeParts = route.split("/");
	const pathParts = pathname.split("/");
	if (routeParts.length !== pathParts.length) return false;
	return routeParts.every((part, i) => {
		if (part.startsWith(":")) return true;
		return part === pathParts[i];
	});
}

//#endregion
export { router_default as default };