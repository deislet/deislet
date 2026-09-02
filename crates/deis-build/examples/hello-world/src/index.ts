// The smallest handler there is: one route, one response.
//
// The id used to come from a bare `import { v4 } from 'uuid'`, which no target
// this compiler builds for can resolve. The three bundled ones refused it -
// nothing carries a module the bundler could not find - and the deno target
// built green and produced a program Deno refuses to load, because a bare
// specifier is not a path and the generated import map maps nothing to it. An
// example that cannot be deployed anywhere is worse than no example.
//
// `crypto.randomUUID` is a Web API and is present on all four: Cloudflare
// Workers, Deno Deploy, EdgeOne, and this platform's own runtime, where
// `js/bootstrap.js` binds Web Crypto onto the isolate's globals. A project that
// does need a package should write it the way its target resolves one - a
// `npm:` specifier for deno, a vendored copy or a real `node_modules` for the
// bundled three.
export default async function handler(_context: any) {
  const id = crypto.randomUUID();
  return new Response(`Hello World! ID: ${id}`, {
    headers: { 'content-type': 'text/plain' }
  });
}
