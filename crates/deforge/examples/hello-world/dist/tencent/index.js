//#region dist/tencent/_deforge_internal/adapters/context-builder.ts
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
//#region node_modules/uuid/dist/esm-browser/rng.js
let getRandomValues;
const rnds8 = new Uint8Array(16);
function rng() {
	if (!getRandomValues) {
		getRandomValues = typeof crypto !== "undefined" && crypto.getRandomValues && crypto.getRandomValues.bind(crypto);
		if (!getRandomValues) throw new Error("crypto.getRandomValues() not supported. See https://github.com/uuidjs/uuid#getrandomvalues-not-supported");
	}
	return getRandomValues(rnds8);
}

//#endregion
//#region node_modules/uuid/dist/esm-browser/stringify.js
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
//#region node_modules/uuid/dist/esm-browser/native.js
const randomUUID = typeof crypto !== "undefined" && crypto.randomUUID && crypto.randomUUID.bind(crypto);
var native_default = { randomUUID };

//#endregion
//#region node_modules/uuid/dist/esm-browser/v4.js
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
//#region dist/tencent/src/index.ts
async function handler(context) {
	const id = v4_default();
	return new Response(`Hello World! ID: ${id}`, { headers: { "content-type": "text/plain" } });
}

//#endregion
//#region dist/tencent/_deforge_internal/router.ts
/**
* Tencent EdgeOne Main Entry
* Auto-generated router
*/
var router_default = { async fetch(request, event) {
	const buildContext = createContextBuilder(event);
	const pathname = new URL(request.url).pathname;
	if (matchRoute(pathname, "/")) return handler(buildContext(request, "/"));
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
