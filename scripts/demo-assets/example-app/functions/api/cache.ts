// The Cache API: the default cache, a cache opened by name, and what the
// platform says about how long either of them lasts.
//
// The two caches are written under the *same* key on purpose. A named cache
// that could answer a `caches.default` read with the wrong page would be a
// correctness bug an application could not see, so the point of the check is
// that each name gets its own answer.
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
  await caches.default.put(key, new Response("default-copy"));
  await pages.put(key, new Response("pages-copy"));

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
