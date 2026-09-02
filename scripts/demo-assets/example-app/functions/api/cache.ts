// The Cache API: the default cache, a cache opened by name, and what the
// platform says about how long either of them lasts.
//
// The two caches are written under the *same* key on purpose. A named cache
// that could answer a `caches.default` read with the wrong page would be a
// correctness bug an application could not see, so the point of the check is
// that each name gets its own answer.
//
// The two puts that are meant to be kept say so with `{ forAnyCaller: true }`,
// and one that is not is made deliberately, so this route shows both halves of
// what `put` answers. A refusal is an answer here rather than an exception -
// see `notStored` in the runtime - because it is the ordinary outcome for a
// perfectly ordinary handler, and an uncaught one would throw away an answer
// the handler had already computed.
//
// `caches.scope` is the honest half. This platform's cache lives in the
// isolate's own memory: it is gone when a new version is deployed, and it is
// not shared with the other nodes of the installation. That is a decision, not
// a gap - a cache shared across nodes would be a storage service with a
// storage service's failure modes - and `scope.id` is what makes it checkable
// rather than something an author has to have read. Two requests that see the
// same id were served by the same store; an id that changed means everything
// cached went with the old isolate, which is a different thing from an entry
// having been evicted.
export default async function handler(context: any): Promise<Response> {
  const key = "https://cache.example.invalid/page";

  const pages = await caches.open("pages");
  // `forAnyCaller` is the statement this platform asks for, and these two
  // entries are exactly what it is for: a fixed string that does not depend on
  // who asked for it. Without it, whether the put is kept depends on the
  // caller - a browser sending a session cookie is a caller this platform
  // cannot identify, and an entry every later request would read may not be
  // filed under one. Said on the call, because a header cannot say it: an
  // upstream, a framework or a copied line can all produce `Cache-Control:
  // public`, and none of them can produce a third argument in this file.
  const keptDefault = await caches.default.put(
    key,
    new Response("default-copy"),
    { forAnyCaller: true },
  );
  const keptPages = await pages.put(key, new Response("pages-copy"), {
    forAnyCaller: true,
  });

  // The other half of the same API, shown rather than described: a put whose
  // entry this platform will not keep. The key here carries a header that
  // could name whoever sent it, nothing files the entry under that header, and
  // no statement says whose answer it is - so the entry is not stored, and the
  // report says why. It is an answer and not an exception on purpose: a cache
  // write that could not be kept is not a reason to fail the request the cache
  // was there to make faster, and a handler that treated it as one would
  // answer 500 for every caller who happened to carry a cookie.
  const declined = await caches.default.put(
    new Request("https://cache.example.invalid/for-one-caller", {
      headers: { "x-account": "alice" },
    }),
    new Response("one-caller-copy"),
  );

  const fromDefault = await caches.default.match(key);
  const fromPages = await pages.match(key);

  // Emptying one name must leave the other alone.
  const emptied = await caches.delete("pages");
  const afterDelete = await pages.match(key);
  const survived = await caches.default.match(key);

  return new Response(
    JSON.stringify({
      fromDefault: await fromDefault.text(),
      fromPages: await fromPages.text(),
      storedDefault: keptDefault.stored,
      storedPages: keptPages.stored,
      // Null when this platform kept it, which would mean the rule below had
      // stopped being enforced.
      declinedReason: declined.stored ? null : declined.reason,
      emptied,
      namedCacheEmptyAfterDelete: afterDelete === undefined,
      defaultSurvived: await survived.text(),
      scopeKind: caches.scope.kind,
      scopeId: caches.scope.id,
      maxEntries: caches.scope.maxEntries,
    }),
    { headers: { "content-type": "application/json; charset=utf-8" } },
  );
}
