// The pattern the Cache API exists for, asked by two callers instead of one.
//
// Look in the cache, fall through when it misses, put the answer back. Three
// lines of ordinary code, and also the exact shape in which one caller's page
// is handed to the next one: the store is keyed by the address, every later
// request to this application reads it, and what decides whether the second
// caller gets the first caller's body is whether the entry records whose
// answer it was.
//
// Every answer here names the caller it was produced for, so a check outside
// can put two callers to the same address and compare what comes back, rather
// than take this handler's word for what it did.
//
// `?mode=` picks how the handler records whose answer it is:
//
//   vary  the correct one. The response carries `Vary: Authorization`, so the
//         entry is filed under the credential it was produced for and a caller
//         sending a different credential -- or none -- misses and is answered
//         afresh.
//   bare  the careless one. A credentialed answer whose `Cache-Control` says
//         nothing about whose it is. RFC 9111 section 3.5 forbids a shared
//         cache to reuse such a response, so the platform has to refuse the
//         `put` and say why. The refusal is reported here rather than
//         swallowed: a `put` that quietly does nothing leaves a handler
//         believing it has a cache, and this handler would rather say it has
//         none.
//
// The refusal arrives as `put`'s own answer - `{ stored, reason }` - rather
// than as an exception, because it is the ordinary outcome and not a mistake
// in this file: whether it happens depends on what the caller sent. The
// try/catch is still here, for the other kind of refusal, the kind that is a
// mistake in this file and is the same on every request: an option this cache
// does not honour, a key that is not a GET.
export default async function handler(context: any): Promise<Response> {
  const request = context.request;
  const url = new URL(request.url);
  const mode = url.searchParams.get("mode") === "bare" ? "bare" : "vary";
  // The credential as sent, not a parse of it: two callers are different here
  // exactly when they present different bytes, which is the same question the
  // cache's own variant matching asks.
  const credential = request.headers.get("authorization");
  const caller = credential === null ? "anonymous" : credential;

  const cache = caches.default;
  // The key is the request being answered, which carries the credential -- or
  // carries the truthful absence of one. A key built as `new Request(url)` to
  // tidy the address away would carry neither, and an entry filed under
  // "sent no Authorization" is the variant every anonymous caller matches.
  const hit = await cache.match(request);
  if (hit) {
    return json({
      caller,
      hit: true,
      served: await hit.text(),
      stored: false,
      refused: null,
      scopeId: caches.scope.id,
    });
  }

  const answer = "page-for-" + caller;
  const headers: Record<string, string> = mode === "bare"
    ? {
      "content-type": "text/plain; charset=utf-8",
      // A lifetime, and nothing about who this is for.
      "cache-control": "max-age=60",
    }
    : {
      "content-type": "text/plain; charset=utf-8",
      // `s-maxage` is the shared lifetime; `Vary` is what files the entry
      // under the credential this answer was produced for.
      "cache-control": "s-maxage=60",
      "vary": "authorization",
    };

  let stored = false;
  let refused: string | null = null;
  try {
    const kept = await cache.put(request, new Response(answer, { headers }));
    stored = kept.stored;
    refused = kept.stored ? null : kept.reason;
  } catch (e: any) {
    refused = e && e.message ? String(e.message) : String(e);
  }

  return json({
    caller,
    hit: false,
    served: answer,
    stored,
    refused,
    // Which store answered. Two requests that report the same id were served
    // by the same cache; an id that changed means the isolate was replaced
    // and a miss proves nothing about what the store would have handed back.
    scopeId: caches.scope.id,
  });
}

function json(payload: unknown): Response {
  return new Response(JSON.stringify(payload), {
    headers: { "content-type": "application/json; charset=utf-8" },
  });
}
