// Cache Ops unit tests

#[cfg(test)]
mod tests {
    use crate::runtime::cache_ops::{create_cache_store, init_cache_extension, CacheStore};
    use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions};
    use std::time::Duration;

    /// Turn an ASCII string into the `Uint8Array` `op_cache_set` now takes,
    /// and back again.
    ///
    /// Written out rather than reached for: these run on a bare `JsRuntime`
    /// carrying the cache extension and nothing else, so there is no
    /// `TextEncoder` in the isolate.
    const BYTES: &str = r#"
        const bytes = (s) => {
            const out = new Uint8Array(s.length);
            for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
            return out;
        };
        const text = (u8) => {
            let out = "";
            for (let i = 0; i < u8.length; i++) out += String.fromCharCode(u8[i]);
            return out;
        };
    "#;

    /// Run one script to completion, driving the event loop the async ops
    /// need.
    async fn run(runtime: &mut JsRuntime, code: &str) {
        let result = runtime
            .execute_script("<test>", format!("{BYTES}\n{code}"))
            .unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();
    }

    fn store(runtime: &JsRuntime) -> CacheStore {
        let op_state = runtime.op_state();
        let state = op_state.borrow();
        let cache = state.borrow::<CacheStore>().clone();
        cache
    }

    fn setup_test_runtime() -> JsRuntime {
        let runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_cache_extension()],
            ..Default::default()
        });

        // Inject CacheStore
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        let cache_store: CacheStore = create_cache_store(100, 0); // 100 entries, no TTL
        state.put(cache_store);

        runtime
    }

    #[tokio::test]
    async fn test_cache_set_and_get() {
        let mut runtime = setup_test_runtime();

        // Set and get value
        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set(
                    "default", "test_key", "{\"status\":200}", bytes("test_value"), 0);
            })()
        "#;

        run(&mut runtime, code).await;

        // Verify by directly accessing CacheStore
        let cache = store(&runtime);

        let value = cache
            .entry("default", "test_key")
            .await
            .expect("the entry was written");
        assert_eq!(value.meta, "{\"status\":200}");
        assert_eq!(value.body, b"test_value".to_vec());
    }

    #[tokio::test]
    async fn test_cache_get_nonexistent() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_get("default", "nonexistent_key");
            })()
        "#;

        run(&mut runtime, code).await;

        let cache = store(&runtime);
        assert_eq!(cache.entry("default", "nonexistent_key").await, None);
    }

    #[tokio::test]
    async fn test_cache_delete() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set(
                    "default", "key_to_delete", "{}", bytes("value"), 0);
                await Deno.core.ops.op_cache_delete("default", "key_to_delete");
            })()
        "#;

        run(&mut runtime, code).await;

        let cache = store(&runtime);
        assert_eq!(cache.entry("default", "key_to_delete").await, None);
    }

    /// Two names, one store, and nothing crosses between them.
    ///
    /// This is what `caches.open(name)` is worth: a cache an application opens
    /// for its own purpose must not be able to answer a `caches.default` read
    /// with the wrong page.
    #[tokio::test]
    async fn a_named_cache_cannot_see_the_default_cache_s_entries() {
        let mut runtime = setup_test_runtime();

        run(
            &mut runtime,
            r#"
            (async () => {
                await Deno.core.ops.op_cache_set(
                    "default", "shared_key", "{}", bytes("from-default"), 0);
                await Deno.core.ops.op_cache_set(
                    "pages", "shared_key", "{}", bytes("from-pages"), 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert_eq!(
            cache.entry("default", "shared_key").await.unwrap().body,
            b"from-default".to_vec()
        );
        assert_eq!(
            cache.entry("pages", "shared_key").await.unwrap().body,
            b"from-pages".to_vec()
        );
        // One store, two namespaces: both entries are really there.
        assert_eq!(cache.len().await, 2);
    }

    /// The composition of a name and a key is injective.
    ///
    /// Without the length prefix, `open("ab")` with key `"c"` and `open("a")`
    /// with key `"b:c"` would land on the same string, and an application that
    /// named a cache after anything a visitor controls could read and
    /// overwrite another of its own caches.
    #[tokio::test]
    async fn a_cache_name_containing_the_separator_cannot_reach_another_cache() {
        let mut runtime = setup_test_runtime();

        run(
            &mut runtime,
            r#"
            (async () => {
                await Deno.core.ops.op_cache_set("ab", "c", "{}", bytes("one"), 0);
                await Deno.core.ops.op_cache_set("a", "b:c", "{}", bytes("two"), 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert_eq!(cache.entry("ab", "c").await.unwrap().body, b"one".to_vec());
        assert_eq!(cache.entry("a", "b:c").await.unwrap().body, b"two".to_vec());
        assert_eq!(cache.len().await, 2, "the two keys collided");
    }

    /// `caches.delete(name)` empties one namespace and leaves the others.
    #[tokio::test]
    async fn clearing_one_named_cache_leaves_the_others_alone() {
        let mut runtime = setup_test_runtime();

        run(
            &mut runtime,
            r#"
            (async () => {
                await Deno.core.ops.op_cache_set("pages", "one", "{}", bytes("a"), 0);
                await Deno.core.ops.op_cache_set("pages", "two", "{}", bytes("b"), 0);
                await Deno.core.ops.op_cache_set("default", "kept", "{}", bytes("c"), 0);
                const dropped = await Deno.core.ops.op_cache_clear("pages");
                const none = await Deno.core.ops.op_cache_clear("never_used");
                // Parked in the store rather than read back through a v8
                // scope: the answers under test are values the op returned.
                await Deno.core.ops.op_cache_set(
                    "default", "answers", "{}", bytes(dropped + "," + none), 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert_eq!(
            cache.entry("default", "answers").await.unwrap().body,
            b"2,0".to_vec(),
            "clear must report how many entries it dropped"
        );
        assert_eq!(cache.entry("pages", "one").await, None);
        assert_eq!(cache.entry("pages", "two").await, None);
        assert_eq!(
            cache.entry("default", "kept").await.unwrap().body,
            b"c".to_vec()
        );
    }

    /// The scope an application reads as `caches.scope`.
    ///
    /// The identity is what lets an application tell a miss in the store it
    /// wrote to from a miss in a store that replaced it, so two stores must
    /// never carry the same one.
    #[test]
    fn every_store_has_an_identity_of_its_own_and_reports_the_node_s_numbers() {
        let one = create_cache_store(10_000, 600);
        let two = create_cache_store(10_000, 600);

        assert_eq!(one.scope().kind, "isolate");
        assert_ne!(
            one.scope().id,
            two.scope().id,
            "two isolates' caches must not look like the same store"
        );
        assert_eq!(one.scope().max_entries, 10_000);
        assert_eq!(one.scope().default_ttl_seconds, 600);
        assert!(one.scope().since > 0, "the store has to know when it began");
    }

    #[tokio::test]
    async fn test_cache_multiple_operations() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set("default", "multi1", "{}", bytes("value1"), 0);
                await Deno.core.ops.op_cache_set("default", "multi2", "{}", bytes("value2"), 0);
                await Deno.core.ops.op_cache_set("default", "multi3", "{}", bytes("value3"), 0);
                await Deno.core.ops.op_cache_delete("default", "multi2");
            })()
        "#;

        run(&mut runtime, code).await;

        let cache = store(&runtime);

        let v1 = cache.entry("default", "multi1").await;
        let v2 = cache.entry("default", "multi2").await;
        let v3 = cache.entry("default", "multi3").await;

        assert_eq!(v1.map(|entry| entry.body), Some(b"value1".to_vec()));
        assert!(v2.is_none());
        assert_eq!(v3.map(|entry| entry.body), Some(b"value3".to_vec()));
    }
    #[tokio::test]
    async fn a_lifetime_the_response_asked_for_is_stored_with_the_entry() {
        // The third argument used to be ignored and documented as "reserved",
        // so every entry lived exactly as long as every other one no matter
        // what its `Cache-Control` said.
        let mut runtime = setup_test_runtime();

        run(
            &mut runtime,
            r#"
            (async () => {
                await Deno.core.ops.op_cache_set("default", "with_ttl", "{}", bytes("value"), 120);
                await Deno.core.ops.op_cache_set("default", "without_ttl", "{}", bytes("value"), 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert_eq!(
            cache.entry("default", "with_ttl").await.unwrap().ttl,
            Some(Duration::from_secs(120))
        );
        // Zero is not "expire immediately"; it is "the response named no
        // lifetime", and the store's own default applies.
        assert_eq!(
            cache.entry("default", "without_ttl").await.unwrap().ttl,
            None
        );
    }

    #[test]
    fn an_entry_expires_on_its_own_lifetime_and_falls_back_to_the_default() {
        // The policy itself, asked directly, so the assertion is about which
        // duration is chosen and not about waiting for one to elapse.
        use crate::runtime::cache_ops::{CacheEntry, ResponseFreshness};
        use moka::Expiry;
        use std::time::Instant;

        let policy = ResponseFreshness {
            default_ttl: Some(Duration::from_secs(600)),
        };
        let now = Instant::now();
        let key = "http://example/x".to_string();

        let asked = CacheEntry {
            meta: "{}".to_string(),
            body: b"body".to_vec(),
            ttl: Some(Duration::from_secs(30)),
        };
        let silent = CacheEntry {
            meta: "{}".to_string(),
            body: b"body".to_vec(),
            ttl: None,
        };

        assert_eq!(
            policy.expire_after_create(&key, &asked, now),
            Some(Duration::from_secs(30)),
            "a response that named a max-age keeps it"
        );
        assert_eq!(
            policy.expire_after_create(&key, &silent, now),
            Some(Duration::from_secs(600)),
            "a response that named none gets the store's default"
        );
        // Writing the same key again restarts the clock rather than
        // inheriting what was left of the previous response's lifetime.
        assert_eq!(
            policy.expire_after_update(&key, &asked, now, Some(Duration::from_secs(1))),
            Some(Duration::from_secs(30))
        );
        // Reading must not extend freshness: an HTTP cache entry goes stale a
        // fixed time after it was written, however often it is served.
        assert_eq!(
            policy.expire_after_read(&key, &asked, now, Some(Duration::from_secs(7)), now),
            Some(Duration::from_secs(7))
        );
    }

    #[tokio::test]
    async fn delete_says_whether_there_was_anything_to_delete() {
        // The Cache API's `delete()` resolves to that answer, and an
        // unconditional `true` told an application it had invalidated
        // something it never had.
        let mut runtime = setup_test_runtime();

        run(
            &mut runtime,
            r#"
            (async () => {
                await Deno.core.ops.op_cache_set("default", "present", "{}", bytes("value"), 0);
                const removedPresent =
                    await Deno.core.ops.op_cache_delete("default", "present");
                const removedAbsent =
                    await Deno.core.ops.op_cache_delete("default", "never_written");
                // Parked in the cache itself rather than read back through a
                // v8 scope: the answer under test is a value the op returned.
                await Deno.core.ops.op_cache_set(
                    "default", "answers", "{}", bytes(removedPresent + "," + removedAbsent), 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert_eq!(
            cache.entry("default", "answers").await.unwrap().body,
            b"true,false".to_vec(),
            "delete() must report a hit as true and a miss as false"
        );
        assert!(cache.entry("default", "present").await.is_none());
    }

    /// A cached body that is not text comes back byte for byte.
    ///
    /// `caches.default` is in every isolate whether the application declared
    /// anything or not, so this is the path an image or a protobuf takes.
    /// `put` used to read the body with `response.clone().text()` and `match`
    /// rebuilt it from that string, so every byte in `0x80..=0xff` came back
    /// as the UTF-8 replacement character `ef bf bd` -- an application caching
    /// a PNG served mojibake and nothing raised an error. Driven through a
    /// real `AppRuntime` because the corruption was in the JavaScript half of
    /// the Cache API, not in the store.
    #[tokio::test]
    async fn a_cached_body_that_is_not_text_survives_the_round_trip() {
        use crate::runtime::isolate::{AppRuntime, IsolateSpec};
        use crate::runtime::{ServiceRouter, ServiceWiring};
        use std::collections::HashMap;

        let code = r#"
            const ALL = new Uint8Array(256);
            for (let i = 0; i < 256; i++) ALL[i] = i;
            const hex = (u8) => {
                let out = "";
                for (let i = 0; i < u8.length; i++) out += u8[i].toString(16).padStart(2, "0");
                return out;
            };

            export default async function handler() {
                const key = "https://example.invalid/an-image.png";
                await caches.default.put(
                    key,
                    new Response(ALL, { headers: { "content-type": "image/png" } })
                );
                const hit = await caches.default.match(key);
                const back = new Uint8Array(await hit.arrayBuffer());

                // An empty body is its own case: a cached 204 may not carry
                // one, and rebuilding it from a zero-length buffer would throw
                // inside `match` rather than answering a miss.
                await caches.default.put(
                    "https://example.invalid/nothing",
                    new Response(null, { status: 204 })
                );
                const empty = await caches.default.match("https://example.invalid/nothing");

                return new Response(JSON.stringify({
                    expected: hex(ALL),
                    cached: hex(back),
                    contentType: hit.headers.get("content-type"),
                    status: hit.status,
                    emptyStatus: empty.status,
                    emptyLength: (await empty.arrayBuffer()).byteLength,
                }));
            }
        "#;

        let mut runtime = AppRuntime::new(
            "cache_bytes_app.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
            // No pool behind this isolate, so every service call it makes is
            // refused as "not deployed on this node" -- which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (status, _headers, body) = rx.await.unwrap().unwrap();
        let body = String::from_utf8(body.into_bytes().unwrap_or_default()).unwrap();
        assert_eq!(status, 200, "the handler failed: {body}");

        let seen: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            seen["cached"], seen["expected"],
            "the cache did not hand back the bytes it was given"
        );
        assert_eq!(seen["contentType"], "image/png");
        assert_eq!(seen["status"], 200);
        assert_eq!(seen["emptyStatus"], 204);
        assert_eq!(seen["emptyLength"], 0);
    }

    /// `caches.open`, `caches.delete` and `caches.scope`, from an application.
    ///
    /// Driven through a real `AppRuntime` because the whole of this feature is
    /// in the JavaScript half: the ops only see a name and a key, and what has
    /// to be true is that an application reaching for `caches.open("pages")`
    /// gets a cache that does not answer `caches.default`'s reads, that
    /// `caches.delete` empties it, and that `caches.scope` says what kind of
    /// cache the application is holding before it depends on one.
    #[tokio::test]
    async fn an_application_opens_a_named_cache_and_can_see_what_kind_of_cache_it_is() {
        use crate::runtime::isolate::{AppRuntime, IsolateSpec};
        use crate::runtime::{ServiceRouter, ServiceWiring};
        use std::collections::HashMap;

        let code = r#"
            export default async function handler() {
                const key = "https://example.invalid/page";
                const pages = await caches.open("pages");

                await caches.default.put(key, new Response("from-default"));
                await pages.put(key, new Response("from-pages"));

                const fromDefault = await caches.default.match(key);
                const fromPages = await pages.match(key);

                // Emptying one name must leave the other name's entry alone.
                const emptied = await caches.delete("pages");
                const neverUsed = await caches.delete("no-such-cache");
                const afterDelete = await pages.match(key);
                const stillThere = await caches.default.match(key);

                return new Response(JSON.stringify({
                    fromDefault: await fromDefault.text(),
                    fromPages: await fromPages.text(),
                    emptied,
                    neverUsed,
                    afterDelete: afterDelete === undefined,
                    stillThere: await stillThere.text(),
                    scopeKind: caches.scope.kind,
                    scopeHasId: typeof caches.scope.id === "string"
                        && caches.scope.id.length > 0,
                    scopeStable: caches.scope.id === caches.scope.id,
                    maxEntries: caches.scope.maxEntries,
                }));
            }
        "#;

        let mut runtime = AppRuntime::new(
            "named_cache_app.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
            // No pool behind this isolate, so every service call it makes is
            // refused as "not deployed on this node" -- which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (status, _headers, body) = rx.await.unwrap().unwrap();
        let body = String::from_utf8(body.into_bytes().unwrap_or_default()).unwrap();
        assert_eq!(status, 200, "the handler failed: {body}");

        let seen: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(seen["fromDefault"], "from-default");
        assert_eq!(seen["fromPages"], "from-pages");
        assert_eq!(
            seen["emptied"], true,
            "caches.delete must report the cache it emptied"
        );
        assert_eq!(
            seen["neverUsed"], false,
            "a name nothing was ever stored under holds nothing to delete"
        );
        assert_eq!(seen["afterDelete"], true);
        assert_eq!(seen["stillThere"], "from-default");
        assert_eq!(seen["scopeKind"], "isolate");
        assert_eq!(seen["scopeHasId"], true);
        assert_eq!(seen["scopeStable"], true);
        assert_eq!(seen["maxEntries"], 10_000);
    }
}
