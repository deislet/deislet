// The one handler of the fixture the compiler check builds for every vendor.
//
// It is deliberately dull. The example app next door declares queues, cron, KV
// and a service binding, and `deis build` refuses to build it for Deno Deploy
// because Deno Deploy has no queues - correctly, and loudly, which is a
// different thing worth having. So a project that exercises "four targets, one
// source tree" cannot be that one: it has to stay inside what all four hosts
// can actually run, which is a fetch handler and nothing else.
//
// The marker is what makes each artifact traceable to this file. Every vendor's
// output is searched for it, so an adapter that emitted a well-formed stub -
// the right file names, the right configuration, none of the application -
// fails the check rather than passing it.
export default async function handler(_context: any): Promise<Response> {
  return new Response(
    JSON.stringify({ marker: "portable-handler-marker", route: "/api/where" }),
    { status: 200, headers: { "content-type": "application/json; charset=utf-8" } },
  );
}
