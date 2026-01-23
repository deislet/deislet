import crypto from "crypto";

//#region node_modules/uuid/dist/esm-node/rng.js
const rnds8Pool = new Uint8Array(256);
let poolPtr = rnds8Pool.length;
function rng() {
	if (poolPtr > rnds8Pool.length - 16) {
		crypto.randomFillSync(rnds8Pool);
		poolPtr = 0;
	}
	return rnds8Pool.slice(poolPtr, poolPtr += 16);
}

//#endregion
//#region node_modules/uuid/dist/esm-node/stringify.js
/**
* Convert array of 16 byte values to UUID string format of the form:
* XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX
*/
const byteToHex = [];
for (let i = 0; i < 256; ++i) byteToHex.push((i + 256).toString(16).slice(1));
function unsafeStringify(arr, offset = 0) {
	return byteToHex[arr[offset + 0]] + byteToHex[arr[offset + 1]] + byteToHex[arr[offset + 2]] + byteToHex[arr[offset + 3]] + "-" + byteToHex[arr[offset + 4]] + byteToHex[arr[offset + 5]] + "-" + byteToHex[arr[offset + 6]] + byteToHex[arr[offset + 7]] + "-" + byteToHex[arr[offset + 8]] + byteToHex[arr[offset + 9]] + "-" + byteToHex[arr[offset + 10]] + byteToHex[arr[offset + 11]] + byteToHex[arr[offset + 12]] + byteToHex[arr[offset + 13]] + byteToHex[arr[offset + 14]] + byteToHex[arr[offset + 15]];
}

//#endregion
//#region node_modules/uuid/dist/esm-node/native.js
var native_default = { randomUUID: crypto.randomUUID };

//#endregion
//#region node_modules/uuid/dist/esm-node/v4.js
function v4(options, buf, offset) {
	if (native_default.randomUUID && !buf && !options) return native_default.randomUUID();
	options = options || {};
	const rnds = options.random || (options.rng || rng)();
	rnds[6] = rnds[6] & 15 | 64;
	rnds[8] = rnds[8] & 63 | 128;
	if (buf) {
		offset = offset || 0;
		for (let i = 0; i < 16; ++i) buf[offset + i] = rnds[i];
		return buf;
	}
	return unsafeStringify(rnds);
}
var v4_default = v4;

//#endregion
//#region dist/deno/index.ts
/**
* Deno Deploy Handler
* Generated from: /ext/deislet/deforge/examples/hello-world/src/index.ts
* Route: /ext/deislet/deforge/examples/hello-world/src
*/
async function handler(context) {
	const id = v4_default();
	return new Response(`Hello World! ID: ${id}`, { headers: { "content-type": "text/plain" } });
}
async function handler(request) {
	const url = new URL(request.url);
	return handler({
		request,
		env: Deno.env.toObject(),
		params: extractParams$1(url.pathname, "/ext/deislet/deforge/examples/hello-world/src"),
		path: url.pathname,
		method: request.method,
		query: Object.fromEntries(url.searchParams),
		headers: request.headers,
		cookies: parseCookies$1(request),
		services: { kv: void 0 },
		log: {
			debug: (...args) => console.debug(...args),
			info: (...args) => console.info(...args),
			warn: (...args) => console.warn(...args),
			error: (...args) => console.error(...args)
		},
		cache: {
			match: async () => void 0,
			put: async () => {},
			delete: async () => false
		},
		waitUntil: () => {}
	});
}
function extractParams$1(pathname, route) {
	const params = {};
	const routeParts = route.split("/");
	const pathParts = pathname.split("/");
	routeParts.forEach((part, i) => {
		if (part.startsWith(":")) params[part.slice(1)] = pathParts[i] || "";
	});
	return params;
}
function parseCookies$1(request) {
	const cookies = {};
	const cookieHeader = request.headers.get("Cookie");
	if (cookieHeader) cookieHeader.split(";").forEach((cookie) => {
		const [name, ...rest] = cookie.split("=");
		cookies[name.trim()] = rest.join("=").trim();
	});
	return {
		get: (name) => cookies[name],
		set: (name, value) => {
			cookies[name] = value;
		},
		delete: (name) => {
			delete cookies[name];
		},
		getAll: () => ({ ...cookies })
	};
}

//#endregion
//#region dist/deno/adapters/kv-adapter.ts
/**
* Deno KV Adapter
*/
function createKVAdapter(kv) {
	return {
		async get(key) {
			const result = await kv.get([key]);
			return result.value ? String(result.value) : null;
		},
		async getJSON(key) {
			return (await kv.get([key])).value;
		},
		async put(key, value, options) {
			const kvOptions = {};
			if (options?.expirationTtl) kvOptions.expireIn = options.expirationTtl * 1e3;
			await kv.set([key], value, kvOptions);
		},
		async putJSON(key, value, options) {
			const kvOptions = {};
			if (options?.expirationTtl) kvOptions.expireIn = options.expirationTtl * 1e3;
			await kv.set([key], value, kvOptions);
		},
		async delete(key) {
			await kv.delete([key]);
		},
		async list(options) {
			const entries = kv.list(options?.prefix ? { prefix: [options.prefix] } : {});
			const keys = [];
			for await (const entry of entries) keys.push({
				name: entry.key[0],
				metadata: entry.versionstamp
			});
			return {
				keys,
				list_complete: true,
				cursor: void 0
			};
		},
		async exists(key) {
			return (await kv.get([key])).value !== null;
		}
	};
}

//#endregion
//#region dist/deno/adapters/context-builder.ts
/**
* Deno Context Builder
*/
function createContextBuilder(kv) {
	return function buildContext(request, route = "/") {
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
			services: { kv: kv ? createKVAdapter(kv) : void 0 },
			log: {
				debug: (...args) => console.debug(...args),
				info: (...args) => console.info(...args),
				warn: (...args) => console.warn(...args),
				error: (...args) => console.error(...args)
			},
			cache: {
				match: async (req) => caches.open("v1").then((c) => c.match(req)),
				put: async (req, res) => caches.open("v1").then((c) => c.put(req, res)),
				delete: async (req) => caches.open("v1").then((c) => c.delete(req))
			},
			waitUntil: (promise) => {
				promise.catch(console.error);
			}
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
//#region dist/deno/main.ts
/**
* Deno Deploy Main Entry
* Auto-generated router
*/
const buildContext = createContextBuilder(void 0);
Deno.serve(async (request) => {
	if (matchRoute(new URL(request.url).pathname, "/")) return handler(buildContext(request, "/"));
	return new Response("Not Found", { status: 404 });
});
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
console.log("🚀 Deno Deploy server running...");

//#endregion
export {  };
