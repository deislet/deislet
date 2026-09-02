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
        // 100 entries, 1 MiB, no TTL.
        let cache_store: CacheStore = create_cache_store(100, 1024 * 1024, 0);
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
                    "default", "", "test_key", "{\"status\":200}", bytes("test_value"), 0);
            })()
        "#;

        run(&mut runtime, code).await;

        // Verify by directly accessing CacheStore
        let cache = store(&runtime);

        let value = cache
            .entry("default", "", "test_key")
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
                await Deno.core.ops.op_cache_get("default", "", "nonexistent_key");
            })()
        "#;

        run(&mut runtime, code).await;

        let cache = store(&runtime);
        assert_eq!(cache.entry("default", "", "nonexistent_key").await, None);
    }

    #[tokio::test]
    async fn test_cache_delete() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set(
                    "default", "", "key_to_delete", "{}", bytes("value"), 0);
                await Deno.core.ops.op_cache_delete("default", "", "key_to_delete");
            })()
        "#;

        run(&mut runtime, code).await;

        let cache = store(&runtime);
        assert_eq!(cache.entry("default", "", "key_to_delete").await, None);
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
                    "default", "", "shared_key", "{}", bytes("from-default"), 0);
                await Deno.core.ops.op_cache_set(
                    "pages", "", "shared_key", "{}", bytes("from-pages"), 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert_eq!(
            cache.entry("default", "", "shared_key").await.unwrap().body,
            b"from-default".to_vec()
        );
        assert_eq!(
            cache.entry("pages", "", "shared_key").await.unwrap().body,
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
                await Deno.core.ops.op_cache_set("ab", "", "c", "{}", bytes("one"), 0);
                await Deno.core.ops.op_cache_set("a", "", "b:c", "{}", bytes("two"), 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert_eq!(
            cache.entry("ab", "", "c").await.unwrap().body,
            b"one".to_vec()
        );
        assert_eq!(
            cache.entry("a", "", "b:c").await.unwrap().body,
            b"two".to_vec()
        );
        assert_eq!(cache.len().await, 2, "the two keys collided");
    }

    /// The same question one join further in, where the caller an entry is
    /// filed under is joined to the key.
    ///
    /// An application names its callers from its own data -- a tenant, a user
    /// id, a token digest -- so the strings are as attacker-shaped as a cache
    /// name is. Without a length at that join too, a caller who arranged to be
    /// called `4:bob` would read and overwrite the entries filed under `bob`,
    /// which is the disclosure the whole `forCaller` statement exists to
    /// prevent, reached through the composition rather than through the rule.
    #[tokio::test]
    async fn a_caller_name_containing_the_separator_cannot_reach_another_caller() {
        let mut runtime = setup_test_runtime();

        run(
            &mut runtime,
            r#"
            (async () => {
                await Deno.core.ops.op_cache_set("c", "bob", "p", "{}", bytes("bob"), 0);
                await Deno.core.ops.op_cache_set("c", "3:bob", "p", "{}", bytes("forged"), 0);
                await Deno.core.ops.op_cache_set("c", "", "3:bob:p", "{}", bytes("shared"), 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert_eq!(
            cache.entry("c", "bob", "p").await.unwrap().body,
            b"bob".to_vec()
        );
        assert_eq!(
            cache.entry("c", "3:bob", "p").await.unwrap().body,
            b"forged".to_vec()
        );
        assert_eq!(
            cache.entry("c", "", "3:bob:p").await.unwrap().body,
            b"shared".to_vec()
        );
        assert_eq!(cache.len().await, 3, "two of the three keys collided");
    }

    /// `caches.delete(name)` empties one namespace and leaves the others.
    #[tokio::test]
    async fn clearing_one_named_cache_leaves_the_others_alone() {
        let mut runtime = setup_test_runtime();

        run(
            &mut runtime,
            r#"
            (async () => {
                await Deno.core.ops.op_cache_set("pages", "", "one", "{}", bytes("a"), 0);
                await Deno.core.ops.op_cache_set("pages", "", "two", "{}", bytes("b"), 0);
                await Deno.core.ops.op_cache_set("default", "", "kept", "{}", bytes("c"), 0);
                const dropped = await Deno.core.ops.op_cache_clear("pages");
                const none = await Deno.core.ops.op_cache_clear("never_used");
                // Parked in the store rather than read back through a v8
                // scope: the answers under test are values the op returned.
                await Deno.core.ops.op_cache_set(
                    "default", "", "answers", "{}", bytes(dropped + "," + none), 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert_eq!(
            cache.entry("default", "", "answers").await.unwrap().body,
            b"2,0".to_vec(),
            "clear must report how many entries it dropped"
        );
        assert_eq!(cache.entry("pages", "", "one").await, None);
        assert_eq!(cache.entry("pages", "", "two").await, None);
        assert_eq!(
            cache.entry("default", "", "kept").await.unwrap().body,
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
        let one = create_cache_store(10_000, 64 * 1024 * 1024, 600);
        let two = create_cache_store(10_000, 64 * 1024 * 1024, 600);

        assert_eq!(one.scope().kind, "isolate");
        assert_ne!(
            one.scope().id,
            two.scope().id,
            "two isolates' caches must not look like the same store"
        );
        assert_eq!(one.scope().max_entries, 10_000);
        assert_eq!(one.scope().default_ttl_seconds, 600);
        // Both ceilings are published, because an application deciding what
        // is worth caching needs the one counted in bytes most of all.
        assert_eq!(one.scope().max_bytes, 64 * 1024 * 1024);
        assert_eq!(one.scope().max_entry_bytes, 16 * 1024 * 1024);
        assert!(one.scope().since > 0, "the store has to know when it began");
    }

    #[tokio::test]
    async fn test_cache_multiple_operations() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set("default", "", "multi1", "{}", bytes("value1"), 0);
                await Deno.core.ops.op_cache_set("default", "", "multi2", "{}", bytes("value2"), 0);
                await Deno.core.ops.op_cache_set("default", "", "multi3", "{}", bytes("value3"), 0);
                await Deno.core.ops.op_cache_delete("default", "", "multi2");
            })()
        "#;

        run(&mut runtime, code).await;

        let cache = store(&runtime);

        let v1 = cache.entry("default", "", "multi1").await;
        let v2 = cache.entry("default", "", "multi2").await;
        let v3 = cache.entry("default", "", "multi3").await;

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
                await Deno.core.ops.op_cache_set("default", "", "with_ttl", "{}", bytes("value"), 120);
                await Deno.core.ops.op_cache_set("default", "", "without_ttl", "{}", bytes("value"), 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert_eq!(
            cache.entry("default", "", "with_ttl").await.unwrap().ttl,
            Some(Duration::from_secs(120))
        );
        // Zero is not "expire immediately"; it is "the response named no
        // lifetime", and the store's own default applies.
        assert_eq!(
            cache.entry("default", "", "without_ttl").await.unwrap().ttl,
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
                await Deno.core.ops.op_cache_set("default", "", "present", "{}", bytes("value"), 0);
                const removedPresent =
                    await Deno.core.ops.op_cache_delete("default", "", "present");
                const removedAbsent =
                    await Deno.core.ops.op_cache_delete("default", "", "never_written");
                // Parked in the cache itself rather than read back through a
                // v8 scope: the answer under test is a value the op returned.
                await Deno.core.ops.op_cache_set(
                    "default", "", "answers", "{}", bytes(removedPresent + "," + removedAbsent), 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert_eq!(
            cache.entry("default", "", "answers").await.unwrap().body,
            b"true,false".to_vec(),
            "delete() must report a hit as true and a miss as false"
        );
        assert!(cache.entry("default", "", "present").await.is_none());
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
                Vec::new(),
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
                Vec::new(),
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

    /// The ceiling that actually bounds what a tenant's cache costs the node.
    ///
    /// Entries alone did not: this store is Rust memory beside the isolate,
    /// outside the V8 heap `memoryMb` governs, so ten thousand entries of a
    /// megabyte each was ten gigabytes held by an application whose own memory
    /// limit was never crossed and whose isolate reported nothing. Two hundred
    /// one-megabyte responses -- two per cent of the entry ceiling -- moved
    /// this process's resident memory by 206 MiB.
    #[tokio::test]
    async fn the_store_evicts_on_bytes_rather_than_growing_past_its_ceiling() {
        let mut runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_cache_extension()],
            ..Default::default()
        });
        {
            let op_state = runtime.op_state();
            let mut state = op_state.borrow_mut();
            // A megabyte, and an entry ceiling far above what a megabyte of
            // these entries can reach, so what is under test is the bytes.
            let cache_store: CacheStore = create_cache_store(1_000, 1024 * 1024, 0);
            state.put(cache_store);
        }

        // Sixteen responses of 128 KiB: two megabytes into a one-megabyte
        // store.
        run(
            &mut runtime,
            r#"
            (async () => {
                const body = new Uint8Array(128 * 1024);
                for (let i = 0; i < 16; i++) {
                    await Deno.core.ops.op_cache_set("default", "", "k" + i, "{}", body, 0);
                }
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        let held = cache.held_bytes().await;
        assert!(
            held <= 1024 * 1024,
            "the store holds {held} bytes, past its own one-megabyte ceiling"
        );
        assert!(
            cache.len().await < 16,
            "every entry survived, so nothing was evicted and the ceiling did nothing"
        );
    }

    /// The entry ceiling survives being counted in bytes.
    ///
    /// One counter enforces both, by charging every entry at least its share
    /// of the store; without that floor an application could hold millions of
    /// empty responses inside a byte ceiling it never approached, and an entry
    /// costs more than its body -- a key, three allocations and the store's own
    /// bookkeeping.
    #[tokio::test]
    async fn tiny_entries_still_cannot_exceed_the_entry_ceiling() {
        let mut runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_cache_extension()],
            ..Default::default()
        });
        {
            let op_state = runtime.op_state();
            let mut state = op_state.borrow_mut();
            // Four entries, four kilobytes: an entry costs at least a
            // kilobyte of the ceiling however small its body is.
            let cache_store: CacheStore = create_cache_store(4, 4096, 0);
            state.put(cache_store);
        }

        run(
            &mut runtime,
            r#"
            (async () => {
                for (let i = 0; i < 40; i++) {
                    await Deno.core.ops.op_cache_set("default", "", "k" + i, "{}", bytes("x"), 0);
                }
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert!(
            cache.len().await <= 4,
            "{} entries survived a four-entry store",
            cache.len().await
        );
    }

    /// A response too large for the store is refused, and the application is
    /// told with both numbers in the message.
    ///
    /// Admitting it would be worse than refusing: it could only be made room
    /// for by evicting most of the store, so one `put` would throw away
    /// everything the application had cached, once per call, silently.
    #[tokio::test]
    async fn a_response_too_large_for_the_store_is_refused_and_says_so() {
        let mut runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_cache_extension()],
            ..Default::default()
        });
        {
            let op_state = runtime.op_state();
            let mut state = op_state.borrow_mut();
            // A megabyte, so one entry may hold 256 KiB of it.
            let cache_store: CacheStore = create_cache_store(1_000, 1024 * 1024, 0);
            state.put(cache_store);
        }

        run(
            &mut runtime,
            r#"
            (async () => {
                let outcome = "the oversized response was accepted";
                try {
                    await Deno.core.ops.op_cache_set(
                        "default", "", "big", "{}", new Uint8Array(300 * 1024), 0);
                } catch (e) {
                    outcome = e.message;
                }
                await Deno.core.ops.op_cache_set("default", "", "outcome", "{}", bytes(outcome), 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        let outcome = String::from_utf8(
            cache
                .entry("default", "", "outcome")
                .await
                .expect("the small entry beside it was stored")
                .body,
        )
        .expect("the message is text");
        // The reason alone. Naming the call and saying that nothing was
        // stored is `js/runtime.js`'s half of the sentence, and it does it for
        // every refusal rather than for this one -- see `notStored` there, and
        // `a_refusal_this_platform_judges_does_not_kill_the_invocation` below
        // for what an application receives.
        assert!(
            outcome.starts_with("this entry is 3072")
                && outcome.contains("262144")
                && outcome.contains("1048576"),
            "the refusal has to name what was sent, what one entry may hold and what the whole \
             store holds: {outcome}"
        );
        assert_eq!(
            cache.entry("default", "", "big").await,
            None,
            "the oversized response must not be in the store"
        );
    }
}

/// What the Cache API will and will not store, driven the way an application
/// reaches it.
#[cfg(test)]
mod what_the_cache_refuses_to_store {
    use crate::runtime::isolate::{AppRuntime, IsolateSpec};
    use crate::runtime::{ServiceRouter, ServiceWiring};

    async fn runtime(app: &str, code: &str) -> AppRuntime {
        let mut runtime = AppRuntime::new(
            app.to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
            ServiceRouter::detached(),
        )
        .expect("the app builds");
        runtime.init_module().await.expect("the module loads");
        runtime
    }

    async fn ask(runtime: &mut AppRuntime, headers: &[(&str, &str)]) -> String {
        ask_with(runtime, "GET", headers, None).await
    }

    /// One request, with the method and body the caller names.
    ///
    /// The method is a parameter because the Cache API is defined over GET
    /// alone, and every test here used to send GET: a `put` keyed by a POST
    /// was stored and handed to the next GET of the same URL, and nothing in
    /// this file could see it.
    async fn ask_with(
        runtime: &mut AppRuntime,
        method: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> String {
        ask_at(runtime, method, "/account", headers, body).await
    }

    /// The same, at a path the caller chooses.
    ///
    /// The path is a parameter because a test that steers its handler needs
    /// somewhere to put the instruction, and a header is now the wrong place:
    /// what a request carries is exactly what decides whether its answer may
    /// be stored, so `x-directive: no-store` would be refused for carrying an
    /// unrecognised header rather than for the directive it names, and the
    /// test would pass while checking nothing. A query string steers the
    /// handler without changing what the request says about its caller.
    async fn ask_at(
        runtime: &mut AppRuntime,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> String {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        runtime
            .execute_fetch(
                format!("http://localhost{path}"),
                method.to_string(),
                headers,
                body,
                tx,
            )
            .await
            .expect("the request runs");
        let (status, _headers, body) = rx.await.expect("an answer").expect("an answer");
        let body = String::from_utf8(body.into_bytes().unwrap_or_default()).expect("text");
        assert_eq!(status, 200, "the handler failed: {body}");
        body
    }

    /// One GET at a path, with its answer read as JSON.
    async fn ask_json(
        runtime: &mut AppRuntime,
        path: &str,
        headers: &[(&str, &str)],
    ) -> serde_json::Value {
        let answered = ask_at(runtime, "GET", path, headers, None).await;
        serde_json::from_str(&answered).unwrap_or_else(|e| panic!("json ({e}): {answered}"))
    }

    /// The canonical shape: check the cache, fall through to a handler whose
    /// answer depends on who is asking, cache the answer under the request
    /// itself.
    const VARYING: &str = r#"
        export default async function handler(context) {
            const hit = await caches.default.match(context.request);
            if (hit) return new Response("HIT:" + (await hit.text()));
            const who = context.request.headers.get("authorization") ?? "nobody";
            const fresh = new Response("page-for:" + who, {
                headers: { vary: "authorization" },
            });
            await caches.default.put(context.request, fresh.clone());
            return new Response("MISS:" + (await fresh.text()));
        }
    "#;

    /// A response that says its body depends on a header is only handed back
    /// to a request that carries the same value for it.
    ///
    /// Without this the pattern above -- the one the Cache API exists for --
    /// served the first caller's page to everybody who came after: the entry
    /// was stored under the URL alone and `Vary` appeared nowhere in either
    /// half of the implementation.
    #[tokio::test]
    async fn one_users_page_is_not_served_to_another() {
        let mut runtime = runtime("cache_vary_app.production", VARYING).await;

        let alice = ask(&mut runtime, &[("authorization", "Bearer alice")]).await;
        assert_eq!(alice, "MISS:page-for:Bearer alice");

        let bob = ask(&mut runtime, &[("authorization", "Bearer bob")]).await;
        assert_eq!(
            bob, "MISS:page-for:Bearer bob",
            "Bob was served the page the cache stored for Alice"
        );

        // And the entry is still a cache: the same caller asking again is a
        // hit, which is the whole reason for storing it. Bob's `put` replaced
        // Alice's -- one variant per key -- so it is his that is there.
        let bob_again = ask(&mut runtime, &[("authorization", "Bearer bob")]).await;
        assert_eq!(
            bob_again, "HIT:page-for:Bearer bob",
            "a repeat of the request that was cached has to be a hit"
        );

        // A caller carrying none of the varying headers is a different variant
        // again, and must not be given the one stored for a caller who did.
        let anonymous = ask(&mut runtime, &[]).await;
        assert_eq!(anonymous, "MISS:page-for:nobody");
    }

    /// A varying response stored under a key that is not a `Request`.
    ///
    /// There is nothing to read the varying headers from, so the entry would
    /// be handed to whoever asked for that string next. Refused, and the
    /// message names the fix.
    #[tokio::test]
    async fn a_varying_response_stored_under_a_bare_url_is_refused() {
        let code = r#"
            export default async function handler(context) {
                let refused = null;
                try {
                    const kept = await caches.default.put(
                        "https://example.invalid/account",
                        new Response("page", { headers: { vary: "authorization" } })
                    );
                    if (!kept.stored) refused = kept.reason;
                } catch (e) {
                    refused = e.message;
                }
                const hit = await caches.default.match("https://example.invalid/account");
                return new Response(JSON.stringify({
                    refused,
                    stored: hit === undefined ? null : await hit.text(),
                }));
            }
        "#;
        let mut runtime = runtime("cache_vary_bare_key.production", code).await;
        let answered = ask(&mut runtime, &[]).await;
        let seen: serde_json::Value = serde_json::from_str(&answered).expect("json");
        assert_eq!(
            seen["stored"],
            serde_json::Value::Null,
            "a response whose variant cannot be identified must not be stored"
        );
        let refused = seen["refused"].as_str().unwrap_or_default();
        assert!(
            refused.contains("Pass the Request itself as the key"),
            "the refusal has to name the fix: {refused}"
        );
    }

    /// A varying response stored under a `Request` that carries none of the
    /// headers it varies on.
    ///
    /// `new Request(url)` is a `Request`, so the refusal above -- which asked
    /// what the key *was* -- let it through, and it carries no headers at all,
    /// so the entry was filed as the "carried no Authorization" variant. Every
    /// later caller who also carried none was handed it: one user's
    /// authenticated page served to anonymous clients, which is the exact
    /// failure the refusal was written to prevent, reached through the one
    /// shape a test of the key's type cannot see.
    ///
    /// Rebuilding a key with `new Request(request.url)` is an ordinary thing
    /// to write -- to normalise a URL, to drop a query string, to cache under
    /// a canonical form -- so this is not an exotic path.
    #[tokio::test]
    async fn a_varying_response_stored_under_a_key_that_dropped_the_headers_is_refused() {
        let code = r#"
            const KEY = "https://example.invalid/account";
            export default async function handler(context) {
                const hit = await caches.default.match(new Request(KEY));
                if (hit) {
                    return new Response(JSON.stringify({ served: await hit.text() }));
                }
                const who = context.request.headers.get("authorization") ?? "nobody";
                const fresh = new Response("page-for:" + who, {
                    headers: { vary: "authorization" },
                });
                let refused = null;
                try {
                    const kept = await caches.default.put(new Request(KEY), fresh.clone());
                    if (!kept.stored) refused = kept.reason;
                } catch (e) {
                    refused = e.message;
                }
                return new Response(JSON.stringify({ refused, served: null }));
            }
        "#;
        let mut runtime = runtime("cache_vary_stripped_key.production", code).await;

        let alice: serde_json::Value =
            serde_json::from_str(&ask(&mut runtime, &[("authorization", "Bearer alice")]).await)
                .expect("json");
        let anonymous: serde_json::Value =
            serde_json::from_str(&ask(&mut runtime, &[]).await).expect("json");

        // The consequence first, because it is the whole reason for the rule.
        assert_eq!(
            anonymous["served"],
            serde_json::Value::Null,
            "an anonymous caller was served the page the cache stored for an authenticated one"
        );
        let refused = alice["refused"].as_str().unwrap_or_default();
        assert!(
            refused.contains("carries no value for authorization"),
            "an authenticated page was stored under a key that had dropped the credential \
             it varies on: {alice}"
        );
    }

    /// And the case the refusal must not swallow: a caller who genuinely
    /// carries none of the varying headers, storing their own page under the
    /// request being answered.
    ///
    /// The key is the request itself, so "carries none of them" is the truth
    /// about the response rather than a claim about a stand-in. Refusing this
    /// would mean no anonymous answer to a credential-varying route could ever
    /// be cached, which is most of what a cache in front of such a route is
    /// for.
    #[tokio::test]
    async fn an_anonymous_caller_may_still_cache_the_page_it_was_given() {
        let mut runtime = runtime("cache_vary_anonymous.production", VARYING).await;

        assert_eq!(ask(&mut runtime, &[]).await, "MISS:page-for:nobody");
        assert_eq!(
            ask(&mut runtime, &[]).await,
            "HIT:page-for:nobody",
            "the anonymous variant was refused rather than stored"
        );
        assert_eq!(
            ask(&mut runtime, &[("authorization", "Bearer alice")]).await,
            "MISS:page-for:Bearer alice",
            "the anonymous entry was handed to a caller who carried a credential"
        );
    }

    /// `Vary: *` is the response saying no later request may reuse it.
    #[tokio::test]
    async fn a_response_that_varies_on_everything_is_refused() {
        let code = r#"
            export default async function handler(context) {
                let refused = null;
                try {
                    const kept = await caches.default.put(
                        context.request,
                        new Response("page", { headers: { vary: "*" } })
                    );
                    if (!kept.stored) refused = kept.reason;
                } catch (e) {
                    refused = e.message;
                }
                const hit = await caches.default.match(context.request);
                return new Response(JSON.stringify({
                    refused,
                    stored: hit === undefined ? null : await hit.text(),
                }));
            }
        "#;
        let mut runtime = runtime("cache_vary_star.production", code).await;
        let seen: serde_json::Value =
            serde_json::from_str(&ask(&mut runtime, &[]).await).expect("json");
        assert_eq!(seen["stored"], serde_json::Value::Null);
        assert!(
            seen["refused"]
                .as_str()
                .unwrap_or_default()
                .contains("Vary: *"),
            "the refusal has to name the directive: {seen}"
        );
    }

    #[tokio::test]
    async fn a_response_that_says_do_not_reuse_me_is_not_stored() {
        // The directives live in the handler and the request names one by
        // index, because what a request carries is now what decides whether
        // its answer may be stored at all: an `x-directive` header would be
        // refused for being a name this platform does not recognise, the
        // assertion below would still hold, and the test would have stopped
        // reading `Cache-Control` without anything saying so.
        let code = r#"
            const DIRECTIVES = [
                "no-store",
                "private",
                "no-cache",
                "max-age=0",
                // A shared cache reads `s-maxage` first, so this one is stale
                // on arrival however long `max-age` says.
                "max-age=600, s-maxage=0",
                // The field-name forms. Three regular expressions scanning the
                // same string used to read `private="set-cookie"` as an
                // ordinary header value and store the response.
                "private=\"set-cookie\"",
                "no-cache=\"set-cookie\"",
                // Not the first directive, and spelled the way a real upstream
                // spells it.
                "public, max-age=600, no-store",
            ];
            export default async function handler(context) {
                const which = Number(
                    new URL(context.request.url).searchParams.get("directive")
                );
                const directive = DIRECTIVES[which];
                const key = "https://example.invalid/" + which;
                let refused = null;
                try {
                    const kept = await caches.default.put(key, new Response("secret", {
                        headers: { "cache-control": directive },
                    }));
                    if (!kept.stored) refused = kept.reason;
                } catch (e) {
                    refused = e.message;
                }
                const hit = await caches.default.match(key);
                return new Response(JSON.stringify({
                    directive,
                    refused,
                    stored: hit === undefined ? null : await hit.text(),
                }));
            }
        "#;
        let mut runtime = runtime("cache_directive_app.production", code).await;
        for which in 0..8 {
            let seen = ask_json(&mut runtime, &format!("/account?directive={which}"), &[]).await;
            let directive = seen["directive"].as_str().unwrap_or_default().to_string();
            assert_eq!(
                seen["stored"],
                serde_json::Value::Null,
                "a response carrying Cache-Control: {directive} was stored anyway"
            );
            assert!(
                seen["refused"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("Cache-Control:"),
                "the refusal has to say it was the response's own directive that refused it \
                 ({directive}): {seen}"
            );
        }
    }

    /// A response answering a POST is not stored, and never becomes the answer
    /// to a later GET of the same URL.
    ///
    /// The Cache API is defined over GET: `Cache.put` refuses any other
    /// method, and `Cache.match` never answers one. Both halves were missing
    /// here -- the key was the request's URL and nothing else -- so a handler
    /// that cached the answer to `POST /account` handed that answer to every
    /// later `GET /account`, whoever sent it. Every test in this file sent
    /// GET, which is why the whole dimension was invisible.
    const BY_METHOD: &str = r#"
        export default async function handler(context) {
            const request = context.request;
            if (request.method !== "GET") {
                let refused = null;
                try {
                    const kept = await caches.default.put(
                        request,
                        new Response("answer-to:" + request.method),
                    );
                    if (!kept.stored) refused = kept.reason;
                } catch (e) {
                    refused = e.message;
                }
                const hit = await caches.default.match(request);
                // The third verb, and the one whose answer is easiest to
                // mistake: `false` here means "there was nothing under that
                // key", which is true and is almost never what a handler
                // invalidating a page after a write meant to hear.
                const dropped = await caches.default.delete(request);
                return new Response(JSON.stringify({
                    refused,
                    dropped,
                    ownMatch: hit === undefined ? null : await hit.text(),
                }));
            }
            const hit = await caches.default.match(request);
            return new Response(JSON.stringify({
                served: hit === undefined ? null : await hit.text(),
            }));
        }
    "#;

    #[tokio::test]
    async fn a_post_is_not_cached_and_its_answer_never_reaches_a_get() {
        let mut runtime = runtime("cache_method_app.production", BY_METHOD).await;

        let posted: serde_json::Value =
            serde_json::from_str(&ask_with(&mut runtime, "POST", &[], Some(b"x".to_vec())).await)
                .expect("json");
        let refused = posted["refused"].as_str().unwrap_or_default();
        assert!(
            refused.contains("POST"),
            "a POST's answer was stored, and the caller was not told: {posted}"
        );
        assert_eq!(
            posted["ownMatch"],
            serde_json::Value::Null,
            "a cache keyed by a POST answered that very POST, so something was stored"
        );
        assert_eq!(
            posted["dropped"], false,
            "a delete keyed by a POST reported that it had removed something"
        );

        let got: serde_json::Value =
            serde_json::from_str(&ask(&mut runtime, &[]).await).expect("json");
        assert_eq!(
            got["served"],
            serde_json::Value::Null,
            "a GET was answered with what a POST to the same URL returned"
        );
    }

    /// The same rule for the other methods a handler answers, including the
    /// ones with a body and the one that looks most like a GET.
    #[tokio::test]
    async fn no_method_but_get_may_be_cached() {
        let mut runtime = runtime("cache_method_all.production", BY_METHOD).await;
        for method in ["PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"] {
            let body = match method {
                "PUT" | "PATCH" => Some(b"x".to_vec()),
                _ => None,
            };
            let answered: serde_json::Value =
                serde_json::from_str(&ask_with(&mut runtime, method, &[], body).await)
                    .expect("json");
            assert!(
                answered["refused"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(method),
                "a {method} was accepted as a cache key: {answered}"
            );
        }
        let got: serde_json::Value =
            serde_json::from_str(&ask(&mut runtime, &[]).await).expect("json");
        assert_eq!(
            got["served"],
            serde_json::Value::Null,
            "a GET was answered with what another method returned"
        );
    }

    /// A key that carries *some* of the headers the response varies on.
    ///
    /// The refusal this replaces fired only when the key carried none of them,
    /// so one extra name in `Vary` walked straight past it: the entry was
    /// filed under `accept-language=en, authorization=null` -- the variant for
    /// a caller who sends a language and no credential -- and the next
    /// anonymous caller sending that language was handed the authenticated
    /// page. The property is not "carries at least one"; it is that no value
    /// in the variant may be one this platform inferred from a key that is not
    /// the request being answered.
    #[tokio::test]
    async fn a_key_missing_one_of_several_varying_headers_is_refused() {
        let code = r#"
            const KEY = "https://example.invalid/account";
            const key = () => new Request(KEY, { headers: { "accept-language": "en" } });
            export default async function handler(context) {
                const hit = await caches.default.match(key());
                if (hit) {
                    return new Response(JSON.stringify({ served: await hit.text() }));
                }
                const who = context.request.headers.get("authorization") ?? "nobody";
                const fresh = new Response("page-for:" + who, {
                    headers: { vary: "accept-language, authorization" },
                });
                let refused = null;
                try {
                    const kept = await caches.default.put(key(), fresh.clone());
                    if (!kept.stored) refused = kept.reason;
                } catch (e) {
                    refused = e.message;
                }
                return new Response(JSON.stringify({ refused, served: null }));
            }
        "#;
        let mut runtime = runtime("cache_vary_partial_key.production", code).await;

        let alice: serde_json::Value = serde_json::from_str(
            &ask(
                &mut runtime,
                &[("authorization", "Bearer alice"), ("accept-language", "en")],
            )
            .await,
        )
        .expect("json");
        let anonymous: serde_json::Value =
            serde_json::from_str(&ask(&mut runtime, &[("accept-language", "en")]).await)
                .expect("json");

        assert_eq!(
            anonymous["served"],
            serde_json::Value::Null,
            "an anonymous caller was served the page stored for an authenticated one, because \
             the key carried one of the two varying headers and the refusal asked for none"
        );
        let refused = alice["refused"].as_str().unwrap_or_default();
        assert!(
            refused.contains("authorization"),
            "the refusal has to name the header whose value the key did not carry: {alice}"
        );
    }

    /// A response to a request that carried a credential, with no `Vary` to
    /// file it under.
    ///
    /// Round six: nothing in the entry recorded *who* it was for, so every
    /// later caller of that URL matched it, credential or none. Round seven:
    /// the repair named `Authorization` and offered `public` and `s-maxage`
    /// as ways to say the answer is everyone's -- so a handler that wrote
    /// either of them turned the refusal back into the disclosure, with the
    /// refusal message recommending it.
    ///
    /// Both are closed here by the same sentence, and this test is the pair
    /// of them: the credential is refused whatever the response says about
    /// its lifetime, and the two things that do make it storable are the
    /// response filing the entry under the credential, and the application
    /// saying whose answer it is on the call.
    #[tokio::test]
    async fn an_authenticated_answer_with_no_vary_is_not_stored_for_everyone() {
        let code = r#"
            export default async function handler(context) {
                const request = context.request;
                const url = new URL(request.url);
                const hit = await caches.default.match(request);
                if (hit) {
                    return new Response(JSON.stringify({ served: await hit.text() }));
                }
                const who = request.headers.get("authorization") ?? "nobody";
                const directive = url.searchParams.get("directive");
                const headers = directive ? { "cache-control": directive } : {};
                if (url.searchParams.get("vary")) headers["vary"] = "authorization";
                const forCaller = url.searchParams.get("forCaller");
                let refused = null;
                try {
                    const kept = await caches.default.put(
                        request,
                        new Response("page-for:" + who, { headers }),
                        forCaller ? { forCaller } : undefined,
                    );
                    if (!kept.stored) refused = kept.reason;
                } catch (e) {
                    refused = e.message;
                }
                return new Response(JSON.stringify({ refused, served: null }));
            }
        "#;
        let mut runtime = runtime("cache_authorization_app.production", code).await;
        let alice = &[("authorization", "Bearer alice")][..];

        let stored = ask_json(&mut runtime, "/account", alice).await;
        let anonymous = ask_json(&mut runtime, "/account", &[]).await;
        assert_eq!(
            anonymous["served"],
            serde_json::Value::Null,
            "an anonymous caller was handed the answer to a request that carried a credential"
        );
        assert!(
            stored["refused"]
                .as_str()
                .unwrap_or_default()
                .contains("authorization"),
            "the refusal has to name the credential that made the answer one caller's: {stored}"
        );

        // The directive that used to be the way out. It says how long the
        // answer is good for and nothing about whose it is, so it changes
        // nothing here -- and a caller sending no credential is still not
        // given the credentialed page.
        let public = ask_json(
            &mut runtime,
            "/account?directive=public,%20max-age=60",
            alice,
        )
        .await;
        assert!(
            public["refused"].is_string(),
            "Cache-Control: public was read as permission to store one caller's answer: {public}"
        );
        let after_public =
            ask_json(&mut runtime, "/account?directive=public,%20max-age=60", &[]).await;
        assert_eq!(
            after_public["served"],
            serde_json::Value::Null,
            "an anonymous caller was handed a credentialed answer marked public: {after_public}"
        );

        // And the two that do work, because each one says who the answer is
        // for: the response files the entry under the credential, or the
        // application names the caller in the call itself.
        let varied = ask_json(&mut runtime, "/account?vary=1", alice).await;
        assert_eq!(
            varied["refused"],
            serde_json::Value::Null,
            "a response that files the entry under the credential is storable: {varied}"
        );
        let varied_again = ask_json(&mut runtime, "/account?vary=1", alice).await;
        assert_eq!(
            varied_again["served"], "page-for:Bearer alice",
            "the caller the entry was filed under did not get it back: {varied_again}"
        );
        let stranger = ask_json(&mut runtime, "/account?vary=1", &[]).await;
        assert_eq!(
            stranger["served"],
            serde_json::Value::Null,
            "a caller sending no credential matched the variant filed under one: {stranger}"
        );

        let named = ask_json(&mut runtime, "/account?forCaller=alice", alice).await;
        assert_eq!(
            named["refused"],
            serde_json::Value::Null,
            "an application that named the caller on the call was still refused: {named}"
        );
    }

    /// A 206 is one range of a representation, and this cache can neither
    /// complete it nor match a `Range` against it.
    ///
    /// Stored, it was handed whole to the next request for that URL -- a
    /// request that sent no `Range` at all and had asked for the entire body.
    /// The client is told "206 Partial Content" for five bytes of a hundred it
    /// never asked to be given in pieces.
    #[tokio::test]
    async fn a_partial_response_is_not_stored_and_never_answers_a_whole_request() {
        let code = r#"
            export default async function handler(context) {
                const hit = await caches.default.match(context.request);
                if (hit) {
                    return new Response(JSON.stringify({
                        servedStatus: hit.status,
                        servedBody: await hit.text(),
                    }));
                }
                const status = Number(context.request.headers.get("x-status") ?? "206");
                const range = context.request.headers.get("x-content-range");
                const headers = range ? { "content-range": range } : {};
                // A 304 may not carry a body at all, and the constructor says
                // so before this cache gets a word in.
                const body = status === 304 ? null : "01234";
                let refused = null;
                try {
                    const kept = await caches.default.put(
                        context.request,
                        new Response(body, { status, headers }),
                    );
                    if (!kept.stored) refused = kept.reason;
                } catch (e) {
                    refused = e.message;
                }
                return new Response(JSON.stringify({ refused, servedStatus: null }));
            }
        "#;
        let mut runtime = runtime("cache_partial_app.production", code).await;

        let stored: serde_json::Value = serde_json::from_str(
            &ask(
                &mut runtime,
                &[("x-content-range", "bytes 0-4/100"), ("x-status", "206")],
            )
            .await,
        )
        .expect("json");
        let next: serde_json::Value =
            serde_json::from_str(&ask(&mut runtime, &[]).await).expect("json");
        assert_eq!(
            next["servedStatus"],
            serde_json::Value::Null,
            "a request that sent no Range was answered 206 with somebody else's five bytes"
        );
        assert!(
            stored["refused"]
                .as_str()
                .unwrap_or_default()
                .contains("206"),
            "the refusal has to name the status: {stored}"
        );

        // The sibling: the same partial body wearing a 200. `Content-Range` is
        // what makes a body a range, and a status can be written by hand.
        let mislabelled: serde_json::Value = serde_json::from_str(
            &ask(
                &mut runtime,
                &[("x-content-range", "bytes 0-4/100"), ("x-status", "200")],
            )
            .await,
        )
        .expect("json");
        assert!(
            mislabelled["refused"]
                .as_str()
                .unwrap_or_default()
                .contains("Content-Range"),
            "a body that describes itself as one range of a larger one was stored whole: \
             {mislabelled}"
        );

        // And a 304, which answers a conditional request and nothing else:
        // handed to a request that sent no validator it means "you already
        // have this", to a client that has never seen it.
        let not_modified: serde_json::Value =
            serde_json::from_str(&ask(&mut runtime, &[("x-status", "304")]).await).expect("json");
        assert!(
            not_modified["refused"]
                .as_str()
                .unwrap_or_default()
                .contains("304"),
            "a 304 was stored as though it were a representation: {not_modified}"
        );
    }

    // ---------------------------------------------------------------
    // The rule, exercised as families rather than as names.
    //
    // Four rounds of this cache were repaired by adding the name the round
    // had found: `Set-Cookie` collapsed, then `Vary` unread, then the method
    // unread, then `Authorization`. Each repair was right and each left the
    // next name to be found, because the question the cache asked was "is
    // this one of the things known to be unsafe". The tests below ask the
    // question the cache asks now -- "is this one of the things known to be
    // safe" -- and they ask it with names no list has heard of, because a
    // name nobody thought of is the whole point.
    // ---------------------------------------------------------------

    /// A handler whose answer is one caller's, cached under the request it
    /// answered, with the header that identifies the caller named in the
    /// query string rather than sent as one more header. Where the credential
    /// lives is the variable; that it is a credential is the constant.
    const DASHBOARD: &str = r#"
        export default async function handler(context) {
            const request = context.request;
            const url = new URL(request.url);
            const hit = await caches.default.match(request);
            if (hit) return new Response(JSON.stringify({ served: await hit.text() }));
            const name = url.searchParams.get("credential");
            const who = request.headers.get(name) ?? "nobody";
            const directive = url.searchParams.get("directive");
            const headers = directive ? { "cache-control": directive } : {};
            let refused = null;
            try {
                const kept = await caches.default.put(
                    request,
                    new Response("dashboard-for:" + who, { headers }),
                );
                if (!kept.stored) refused = kept.reason;
            } catch (e) {
                refused = e.message;
            }
            return new Response(JSON.stringify({ refused, served: null }));
        }
    "#;

    /// Every header that can carry a credential, including ones no list of
    /// credentials contains.
    ///
    /// The rule this replaces named `Authorization` and nothing else, so
    /// `Cookie` -- which is how most of the web authenticates -- walked past
    /// it: the page was stored under the bare URL and handed to the next
    /// anonymous caller. So did `Proxy-Authorization`, and so would any name
    /// an application invents for its own session token. The platform cannot
    /// enumerate what a credential is called, so it no longer tries: a header
    /// it does not recognise as one that cannot single a caller out is one
    /// the entry has to be filed under.
    #[tokio::test]
    async fn no_header_that_could_name_a_caller_is_cached_under_the_bare_url() {
        let mut runtime = runtime("cache_credential_family.production", DASHBOARD).await;
        for name in [
            "authorization",
            "cookie",
            "proxy-authorization",
            "x-api-key",
            "x-session-token",
            // The one that matters: a name invented by one application, that
            // no list of credentials will ever contain.
            "x-acme-tenant-ticket",
        ] {
            let path = format!("/account?credential={name}");
            let identified = ask_json(&mut runtime, &path, &[(name, "secret-of-alice")]).await;
            let anonymous = ask_json(&mut runtime, &path, &[]).await;
            assert_eq!(
                anonymous["served"],
                serde_json::Value::Null,
                "a caller sending nothing was handed the page answered for a caller who sent \
                 {name}: {anonymous}"
            );
            assert!(
                identified["refused"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(name),
                "the refusal has to name the header that made the answer one caller's ({name}): \
                 {identified}"
            );
        }
    }

    /// The two directives the old refusal recommended, against the same
    /// family.
    ///
    /// `public` and `s-maxage` used to be accepted as the response saying
    /// "this answer is everyone's", which turned a correctly refused `put`
    /// into the disclosure it had just refused -- and the refusal message
    /// said to add them. Nothing a response can carry says that any more:
    /// a directive is a lifetime, not a permission.
    #[tokio::test]
    async fn saying_public_does_not_make_one_callers_page_everyones() {
        let mut runtime = runtime("cache_public_directive.production", DASHBOARD).await;
        for directive in ["public, max-age=60", "s-maxage=60", "public"] {
            let path =
                format!("/account?credential=cookie&directive={directive}").replace(' ', "%20");
            let identified = ask_json(&mut runtime, &path, &[("cookie", "session=alice")]).await;
            let anonymous = ask_json(&mut runtime, &path, &[]).await;
            assert_eq!(
                anonymous["served"],
                serde_json::Value::Null,
                "Cache-Control: {directive} let one caller's page be stored for everyone: \
                 {anonymous}"
            );
            assert!(
                identified["refused"].is_string(),
                "Cache-Control: {directive} was read as permission to store a credentialed \
                 answer: {identified}"
            );
        }
    }

    /// A response that carries something for the caller who receives it.
    ///
    /// `Set-Cookie` is the one that matters most -- replayed to every later
    /// caller it is session fixation rather than disclosure -- but it is not
    /// the rule. The rule is that a stored response may carry only headers
    /// that describe the representation, so `Authentication-Info`,
    /// `Clear-Site-Data` and a token an application invented a name for are
    /// refused by the same sentence, and so is the next such header anyone
    /// defines.
    #[tokio::test]
    async fn no_response_header_that_carries_one_callers_state_is_stored() {
        let code = r#"
            export default async function handler(context) {
                const request = context.request;
                const url = new URL(request.url);
                const hit = await caches.default.match(request);
                if (hit) return new Response(JSON.stringify({ served: await hit.text() }));
                const name = url.searchParams.get("header");
                const headers = {};
                headers[name] = name === "set-cookie"
                    ? "session=alice; HttpOnly; Path=/"
                    : "one-callers-value";
                const directive = url.searchParams.get("directive");
                if (directive) headers["cache-control"] = directive;
                let refused = null;
                try {
                    const kept = await caches.default.put(request, new Response("page", { headers }));
                    if (!kept.stored) refused = kept.reason;
                } catch (e) {
                    refused = e.message;
                }
                return new Response(JSON.stringify({ refused, served: null }));
            }
        "#;
        let mut runtime = runtime("cache_response_header_family.production", code).await;
        for name in [
            "set-cookie",
            "authentication-info",
            "clear-site-data",
            "www-authenticate",
            // Invented, and exactly as dangerous: a token minted for one
            // caller and replayed to every later one.
            "x-csrf-token",
        ] {
            let path = format!("/account?header={name}");
            let stored = ask_json(&mut runtime, &path, &[]).await;
            let next = ask_json(&mut runtime, &path, &[]).await;
            assert_eq!(
                next["served"],
                serde_json::Value::Null,
                "a later caller was handed a response carrying {name}: {next}"
            );
            assert!(
                stored["refused"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(name),
                "the refusal has to name the header it would not replay ({name}): {stored}"
            );
        }

        // And the remedy the old message recommended does not turn it into a
        // store: a `Set-Cookie` a shared cache may keep is a contradiction
        // however the response spells its lifetime.
        let path = "/account?header=set-cookie&directive=public,%20max-age=60";
        let stored = ask_json(&mut runtime, path, &[]).await;
        let next = ask_json(&mut runtime, path, &[]).await;
        assert_eq!(
            next["served"],
            serde_json::Value::Null,
            "Cache-Control: public turned a refused Set-Cookie into a stored one: {next}"
        );
        assert!(
            stored["refused"].is_string(),
            "Cache-Control: public turned a refused Set-Cookie into a stored one: {stored}"
        );
    }

    /// The freshness spellings that were never read.
    ///
    /// `Cache-Control` was the only field the store consulted, so a response
    /// that said it had gone stale an hour ago in `Expires`, or `no-cache` in
    /// the HTTP/1.0 spelling, was kept for the store's ten-minute default.
    #[tokio::test]
    async fn a_response_that_says_it_is_already_stale_is_not_kept_for_ten_minutes() {
        let code = r#"
            export default async function handler(context) {
                const request = context.request;
                const url = new URL(request.url);
                const hit = await caches.default.match(request);
                if (hit) return new Response(JSON.stringify({ served: await hit.text() }));
                const headers = {};
                switch (url.searchParams.get("stale")) {
                    case "past":
                        headers["expires"] = new Date(Date.now() - 3600000).toUTCString();
                        break;
                    case "zero":
                        // What a framework writes to mean "never cache this",
                        // and what RFC 9111 reads as a date in the past.
                        headers["expires"] = "0";
                        break;
                    case "pragma":
                        headers["pragma"] = "no-cache";
                        break;
                    case "future":
                        headers["expires"] = new Date(Date.now() + 3600000).toUTCString();
                        break;
                }
                let refused = null;
                try {
                    const kept = await caches.default.put(request, new Response("page", { headers }));
                    if (!kept.stored) refused = kept.reason;
                } catch (e) {
                    refused = e.message;
                }
                return new Response(JSON.stringify({ refused, served: null }));
            }
        "#;
        let mut runtime = runtime("cache_expires_pragma.production", code).await;
        for stale in ["past", "zero", "pragma"] {
            let path = format!("/account?stale={stale}");
            let stored = ask_json(&mut runtime, &path, &[]).await;
            let next = ask_json(&mut runtime, &path, &[]).await;
            assert_eq!(
                next["served"],
                serde_json::Value::Null,
                "a response that said it was already stale ({stale}) was served from the cache: \
                 {next}"
            );
            assert!(
                stored["refused"].is_string(),
                "a response that said it was already stale ({stale}) was stored: {stored}"
            );
        }
        // And the other half: a date in the future is a lifetime, so the
        // entry is stored and the next caller gets it.
        let path = "/account?stale=future";
        let stored = ask_json(&mut runtime, path, &[]).await;
        assert_eq!(
            stored["refused"],
            serde_json::Value::Null,
            "an Expires in the future is a lifetime, not a refusal: {stored}"
        );
        let next = ask_json(&mut runtime, path, &[]).await;
        assert_eq!(
            next["served"], "page",
            "an entry whose Expires is an hour away was not there a moment later: {next}"
        );
    }

    /// `delete` answers for the variant the caller has, not for the URL.
    ///
    /// The store holds one variant per key and `delete` removed it by key
    /// alone, so an anonymous caller's `delete` reported `true` -- telling it
    /// that a variant it cannot see exists -- and threw away the entry
    /// belonging to somebody else.
    #[tokio::test]
    async fn delete_answers_for_the_variant_the_caller_has() {
        let code = r#"
            const KEY = "https://example.invalid/dash";
            const keyFor = (request) => {
                const credential = request.headers.get("authorization");
                return credential
                    ? new Request(KEY, { headers: { authorization: credential } })
                    : new Request(KEY);
            };
            export default async function handler(context) {
                const request = context.request;
                const url = new URL(request.url);
                const key = keyFor(request);
                switch (url.searchParams.get("do")) {
                    case "store": {
                        const who = request.headers.get("authorization") ?? "nobody";
                        await caches.default.put(key, new Response("page-for:" + who, {
                            headers: { vary: "authorization" },
                        }));
                        return new Response(JSON.stringify({ stored: true }));
                    }
                    case "drop": {
                        const dropped = await caches.default.delete(key);
                        return new Response(JSON.stringify({ dropped }));
                    }
                    default: {
                        const hit = await caches.default.match(key);
                        return new Response(JSON.stringify({
                            served: hit === undefined ? null : await hit.text(),
                        }));
                    }
                }
            }
        "#;
        let mut runtime = runtime("cache_delete_variant.production", code).await;
        let alice = &[("authorization", "Bearer alice")][..];

        let stored = ask_json(&mut runtime, "/account?do=store", alice).await;
        assert_eq!(stored["stored"], true);

        let dropped = ask_json(&mut runtime, "/account?do=drop", &[]).await;
        assert_eq!(
            dropped["dropped"], false,
            "an anonymous delete reported that it had removed a variant it cannot see: {dropped}"
        );

        let still_there = ask_json(&mut runtime, "/account?do=read", alice).await;
        assert_eq!(
            still_there["served"], "page-for:Bearer alice",
            "an anonymous delete threw away another caller's entry: {still_there}"
        );

        // And the caller who does have the variant still deletes it.
        let dropped = ask_json(&mut runtime, "/account?do=drop", alice).await;
        assert_eq!(dropped["dropped"], true);
        let gone = ask_json(&mut runtime, "/account?do=read", alice).await;
        assert_eq!(gone["served"], serde_json::Value::Null);
    }

    /// The options the specification defines, none of which this cache used
    /// to read.
    ///
    /// `match(request, options)` and `delete(request, options)` took the bag
    /// and dropped it, so `ignoreVary: true` -- which asks for exactly the
    /// disclosure this file exists to prevent -- read as an ordinary match
    /// and was answered as though the caller had not asked for anything. An
    /// option this cache cannot honour is refused by name; the one it can is
    /// honoured.
    #[tokio::test]
    async fn an_option_this_cache_cannot_honour_is_refused_rather_than_dropped() {
        let code = r#"
            export default async function handler(context) {
                const request = context.request;
                const said = async (fn) => {
                    try {
                        await fn();
                        return null;
                    } catch (e) {
                        return e.message;
                    }
                };
                return new Response(JSON.stringify({
                    ignoreVary: await said(() =>
                        caches.default.match(request, { ignoreVary: true })),
                    ignoreSearch: await said(() =>
                        caches.default.match(request, { ignoreSearch: true })),
                    unknown: await said(() =>
                        caches.default.match(request, { ignoreCase: true })),
                    deleteIgnoreVary: await said(() =>
                        caches.default.delete(request, { ignoreVary: true })),
                }));
            }
        "#;
        let mut runtime = runtime("cache_options_bag.production", code).await;
        let seen = ask_json(&mut runtime, "/account", &[]).await;
        for option in ["ignoreVary", "ignoreSearch", "unknown", "deleteIgnoreVary"] {
            assert!(
                seen[option].is_string(),
                "{option} was taken and dropped rather than refused: {seen}"
            );
        }
        assert!(
            seen["ignoreVary"]
                .as_str()
                .unwrap_or_default()
                .contains("ignoreVary"),
            "the refusal has to name the option: {seen}"
        );
    }

    /// `caches.open` cannot be talked into handing back the default cache.
    ///
    /// An application that opens a cache under a name it took from a request
    /// -- a tenant, a locale, a path segment -- would otherwise hand a caller
    /// who sent the name "default" the namespace every other part of the
    /// application writes to through `context.services.cache`.
    #[tokio::test]
    async fn the_default_cache_cannot_be_opened_by_name() {
        let code = r#"
            export default async function handler(context) {
                const name = new URL(context.request.url).searchParams.get("name");
                await caches.default.put("https://example.invalid/p", new Response("the-default"));
                let refused = null;
                let seen = null;
                try {
                    const opened = await caches.open(name);
                    const hit = await opened.match("https://example.invalid/p");
                    seen = hit === undefined ? null : await hit.text();
                } catch (e) {
                    refused = e.message;
                }
                return new Response(JSON.stringify({ refused, seen }));
            }
        "#;
        let mut runtime = runtime("cache_reserved_name.production", code).await;
        let seen = ask_json(&mut runtime, "/account?name=default", &[]).await;
        assert_eq!(
            seen["seen"],
            serde_json::Value::Null,
            "a cache opened under a name from the request read the default cache: {seen}"
        );
        assert!(
            seen["refused"]
                .as_str()
                .unwrap_or_default()
                .contains("default"),
            "the refusal has to say which name is reserved: {seen}"
        );
        // An ordinary name is still an ordinary namespace.
        let ordinary = ask_json(&mut runtime, "/account?name=pages", &[]).await;
        assert_eq!(ordinary["refused"], serde_json::Value::Null);
        assert_eq!(ordinary["seen"], serde_json::Value::Null);
    }

    /// The two statements an application makes on the call rather than in a
    /// header.
    ///
    /// A per-caller answer may be cached, but only when the application says
    /// whose it is -- and it says so in an argument, which no upstream, no
    /// framework and no copied header can produce by accident. The entry is
    /// then filed under that string and handed back only to a `match` that
    /// names the same one.
    #[tokio::test]
    async fn a_per_caller_answer_is_cached_only_under_the_caller_the_application_names() {
        let code = r#"
            const KEY = "https://example.invalid/dash";
            export default async function handler(context) {
                const request = context.request;
                const url = new URL(request.url);
                const caller = url.searchParams.get("caller");
                const options = caller ? { forCaller: caller } : undefined;
                const hit = await caches.default.match(KEY, options);
                if (hit) return new Response(JSON.stringify({ served: await hit.text() }));
                let refused = null;
                try {
                    const kept = await caches.default.put(
                        KEY,
                        new Response("dashboard-for:" + (caller ?? "nobody")),
                        options,
                    );
                    if (!kept.stored) refused = kept.reason;
                } catch (e) {
                    refused = e.message;
                }
                return new Response(JSON.stringify({ refused, served: null }));
            }
        "#;
        let mut runtime = runtime("cache_for_caller.production", code).await;
        let cookie = &[("cookie", "session=alice")][..];

        let alice = ask_json(&mut runtime, "/account?caller=alice", cookie).await;
        assert_eq!(
            alice["refused"],
            serde_json::Value::Null,
            "an application that said whose answer this is was still refused: {alice}"
        );
        let alice_again = ask_json(&mut runtime, "/account?caller=alice", cookie).await;
        assert_eq!(
            alice_again["served"], "dashboard-for:alice",
            "the caller the entry was filed under did not get it back: {alice_again}"
        );
        let bob = ask_json(
            &mut runtime,
            "/account?caller=bob",
            &[("cookie", "session=bob")],
        )
        .await;
        assert_eq!(
            bob["served"],
            serde_json::Value::Null,
            "one caller's entry was handed to another: {bob}"
        );
        let anonymous = ask_json(&mut runtime, "/account", &[]).await;
        assert_eq!(
            anonymous["served"],
            serde_json::Value::Null,
            "a caller naming nobody was handed an entry filed under a caller: {anonymous}"
        );
    }

    /// The other statement: this answer is not one caller's, said inside an
    /// invocation that is serving one.
    ///
    /// Without it a cache is useless to any application whose requests carry
    /// a session, because everything it fetches while serving that request is
    /// refused. With it the application takes the decision, in a line of its
    /// own code, that the answer does not depend on the caller.
    #[tokio::test]
    async fn an_application_may_state_that_an_answer_is_not_one_callers() {
        let code = r#"
            const KEY = "https://example.invalid/config";
            export default async function handler(context) {
                const hit = await caches.default.match(KEY);
                if (hit) return new Response(JSON.stringify({ served: await hit.text() }));
                let refused = null;
                try {
                    const kept = await caches.default.put(
                        KEY,
                        new Response("shared-config"),
                        { forAnyCaller: true },
                    );
                    if (!kept.stored) refused = kept.reason;
                } catch (e) {
                    refused = e.message;
                }
                return new Response(JSON.stringify({ refused, served: null }));
            }
        "#;
        let mut runtime = runtime("cache_for_any_caller.production", code).await;
        let first = ask_json(&mut runtime, "/account", &[("cookie", "session=alice")]).await;
        assert_eq!(
            first["refused"],
            serde_json::Value::Null,
            "an application that stated the answer is everyone's was refused: {first}"
        );
        let second = ask_json(&mut runtime, "/account", &[("cookie", "session=bob")]).await;
        assert_eq!(
            second["served"], "shared-config",
            "the entry the application stated was everyone's was not served: {second}"
        );
    }
}

/// What a `Cache.put` is judged against: the request the code making it is
/// answering, and nothing that some other stretch of code was answering.
///
/// The rule that reads that request was added a round before this module, and
/// it read one module-level variable in `js/runtime.js` that was set when a
/// request built its context and cleared at the top of three entry points.
/// Nothing cleared it when an invocation ended, and four runners that execute
/// application code never set it: a WebSocket message handler, a
/// response-stream producer, a timer, a `waitUntil` left over from an
/// invocation that was cut off. Each of them read whichever request the isolate
/// had served most recently -- so the same `put` was refused when nothing had
/// run in between and *stored* after one unrelated anonymous `GET`, and what it
/// stored was then handed to every later anonymous caller.
///
/// Every test here is a witness for one runner. What they have in common is the
/// shape of the check: run the same application code twice with something
/// different in between, and require the same answer both times.
#[cfg(test)]
mod what_the_cache_is_answering {
    use crate::runtime::isolate::{AppRuntime, IsolateSpec};
    use crate::runtime::queue_client::LeasedMessage;
    use crate::runtime::{ServiceRouter, ServiceWiring};

    async fn runtime(app: &str, code: &str) -> AppRuntime {
        let mut runtime = AppRuntime::new(
            app.to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
            ServiceRouter::detached(),
        )
        .expect("the app builds");
        runtime.init_module().await.expect("the module loads");
        runtime
    }

    /// One request, and the status it answered with.
    ///
    /// The status is returned rather than asserted, because one of the tests
    /// here is about a handler that used to answer 500 and now answers its own
    /// response.
    async fn ask_at(
        runtime: &mut AppRuntime,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> (u16, String) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        runtime
            .execute_fetch(
                format!("http://localhost{path}"),
                method.to_string(),
                headers,
                body,
                tx,
            )
            .await
            .expect("the request runs");
        let (status, _headers, body) = rx.await.expect("an answer").expect("an answer");
        let body = String::from_utf8(body.into_bytes().unwrap_or_default()).expect("text");
        (status, body)
    }

    async fn ask(runtime: &mut AppRuntime, path: &str, headers: &[(&str, &str)]) -> String {
        let (status, body) = ask_at(runtime, "GET", path, headers, None).await;
        assert_eq!(status, 200, "the handler failed: {body}");
        body
    }

    async fn ask_json(
        runtime: &mut AppRuntime,
        path: &str,
        headers: &[(&str, &str)],
    ) -> serde_json::Value {
        let answered = ask(runtime, path, headers).await;
        serde_json::from_str(&answered).unwrap_or_else(|e| panic!("json ({e}): {answered}"))
    }

    /// A socket opened by one caller stays that caller's, whatever the isolate
    /// does between two of its frames.
    ///
    /// This is the reproduction the round named, run three ways. A handler that
    /// caches from a WebSocket `onmessage`:
    ///
    ///   * with nothing in between -- refused, because the caller who opened
    ///     the socket sent a cookie;
    ///   * with one unrelated anonymous `GET` in between -- refused for the
    ///     same reason, which is the assertion that used to fail. The message
    ///     handler read the anonymous request, judged the answer against *it*,
    ///     stored the entry, and every later anonymous caller was handed it;
    ///   * on a socket an anonymous caller opened -- stored, because that
    ///     socket really is anonymous. Without this last one the test would
    ///     pass against a platform that had simply stopped caching from
    ///     WebSocket handlers at all.
    #[tokio::test]
    async fn a_websocket_message_is_judged_against_the_socket_it_arrived_on() {
        let code = r#"
            let pending = null;
            let outcome = null;
            const KEY = "https://example.invalid/from-the-socket";

            export default async function handler(context) {
                const path = new URL(context.request.url).pathname;
                if (path === "/report") {
                    // The put itself is made inside the message handler; only
                    // the recording of what it answered is awaited here, so
                    // that this request cannot be what the put was judged
                    // against.
                    if (pending) { await pending; pending = null; }
                    const hit = await caches.default.match(KEY);
                    return new Response(JSON.stringify({
                        outcome,
                        stored: hit === undefined ? null : await hit.text(),
                    }));
                }
                if (path === "/unrelated") {
                    return new Response("nothing to do with the socket");
                }
                const pair = new WebSocketPair();
                const socket = pair[1];
                socket.addEventListener("message", (event) => {
                    pending = caches.default
                        .put(KEY, new Response("answer-for:" + String(event.data)))
                        .then((kept) => { outcome = kept.stored ? "stored" : kept.reason; });
                });
                return new Response(JSON.stringify({ id: socket.ws_id }));
            }
        "#;
        let mut runtime = runtime("cache_ws_socket.production", code).await;
        let alice = &[("cookie", "session=alice")][..];

        // One: a socket opened by a caller who sent a cookie, and a frame on it
        // with nothing in between.
        let opened = ask_json(&mut runtime, "/open", alice).await;
        let first = opened["id"].as_str().expect("a socket id").to_string();
        runtime
            .dispatch_ws_message(first, "one".to_string())
            .await
            .expect("the frame is delivered");
        let straight_away = ask_json(&mut runtime, "/report", &[]).await;
        let refusal = straight_away["outcome"].as_str().unwrap_or_default();
        assert!(
            refusal.contains("cookie"),
            "a frame on a socket a cookie-carrying caller opened has to be judged against that \
             caller: {straight_away}"
        );

        // Two: the same thing, with one unrelated anonymous request in between.
        // This is the one that used to differ.
        let opened = ask_json(&mut runtime, "/open", alice).await;
        let second = opened["id"].as_str().expect("a socket id").to_string();
        let _ = ask(&mut runtime, "/unrelated", &[]).await;
        runtime
            .dispatch_ws_message(second, "two".to_string())
            .await
            .expect("the frame is delivered");
        let after_a_stranger = ask_json(&mut runtime, "/report", &[]).await;
        assert_eq!(
            after_a_stranger["outcome"].as_str().unwrap_or_default(),
            refusal,
            "one unrelated anonymous request changed what the socket's own frame was judged \
             against: {after_a_stranger}"
        );
        assert_eq!(
            after_a_stranger["stored"],
            serde_json::Value::Null,
            "the credentialed caller's answer was stored where every later anonymous caller \
             would be handed it: {after_a_stranger}"
        );

        // Three: a socket that really was opened anonymously. Its frames may
        // cache, or the two assertions above would hold on a platform that had
        // stopped caching from WebSocket handlers altogether.
        let opened = ask_json(&mut runtime, "/open", &[]).await;
        let third = opened["id"].as_str().expect("a socket id").to_string();
        runtime
            .dispatch_ws_message(third, "three".to_string())
            .await
            .expect("the frame is delivered");
        let anonymous = ask_json(&mut runtime, "/report", &[]).await;
        assert_eq!(
            anonymous["outcome"], "stored",
            "a frame on a socket nobody identified themselves to open may still be cached: \
             {anonymous}"
        );
        assert_eq!(anonymous["stored"], "answer-for:three");
    }

    /// The method of the request being answered, which the key does not name.
    ///
    /// `cache_ops.rs` already states the rule for the key: an entry is the
    /// answer to one GET. The other half was missing -- a handler answering a
    /// POST from its body, caching the answer under the URL, filed one caller's
    /// result where every later GET of that URL matched it.
    #[tokio::test]
    async fn an_answer_to_a_post_is_not_filed_under_a_get_of_the_same_url() {
        let code = r#"
            const KEY = "https://example.invalid/answer";
            export default async function handler(context) {
                const request = context.request;
                if (request.method === "GET") {
                    const hit = await caches.default.match(KEY);
                    return new Response(hit ? "HIT:" + (await hit.text()) : "MISS");
                }
                const who = await request.text();
                const answer = "answer-for-" + who;
                const kept = await caches.default.put(KEY, new Response(answer));
                return new Response(JSON.stringify({
                    answer,
                    refused: kept.stored ? null : kept.reason,
                }));
            }
        "#;
        let mut runtime = runtime("cache_post_answer.production", code).await;

        let (status, body) = ask_at(
            &mut runtime,
            "POST",
            "/answer",
            &[],
            Some(b"alice".to_vec()),
        )
        .await;
        assert_eq!(status, 200, "the handler failed: {body}");
        let posted: serde_json::Value = serde_json::from_str(&body).expect("json");
        let refused = posted["refused"].as_str().unwrap_or_default();
        assert!(
            refused.contains("is a POST"),
            "the refusal has to name the method of the request being answered: {posted}"
        );

        // The consequence, which is the whole reason for the rule: the next
        // caller asks the same address with a GET and is answered afresh.
        let bob = ask(&mut runtime, "/answer", &[]).await;
        assert_eq!(
            bob, "MISS",
            "a GET of the URL was handed the result of somebody else's POST"
        );
    }

    /// A `put` this platform will not keep does not throw away the answer the
    /// handler had already computed.
    ///
    /// The rule that closed the previous round makes a refusal the ordinary
    /// outcome: any handler serving a browser that sends a session cookie is
    /// refused unless it says whose answer it is. While that threw, an
    /// application that did not catch it answered 500 -- and this platform's
    /// own example did not catch it, because the thing that exercised it sent
    /// no cookies.
    #[tokio::test]
    async fn a_refusal_this_platform_judges_does_not_kill_the_invocation() {
        let code = r#"
            export default async function handler(context) {
                if (new URL(context.request.url).pathname === "/read") {
                    const hit = await caches.default.match("https://example.invalid/page");
                    return new Response(hit === undefined ? "MISS" : "HIT");
                }
                const answer = "the-handlers-own-answer";
                // No try/catch, which is what the platform's own example looked
                // like: an ordinary put of an ordinary response.
                await caches.default.put(
                    "https://example.invalid/page",
                    new Response(answer),
                );
                return new Response(answer);
            }
        "#;
        let mut runtime = runtime("cache_uncaught_refusal.production", code).await;

        let (status, body) = ask_at(
            &mut runtime,
            "GET",
            "/page",
            &[("cookie", "session=alice")],
            None,
        )
        .await;
        assert_eq!(
            status, 200,
            "a cache write this platform would not keep failed the request the cache was there \
             to make faster: {body}"
        );
        assert_eq!(body, "the-handlers-own-answer");

        // And it really was refused: the entry a later anonymous caller would
        // have been handed is not there. Surviving the refusal must not mean
        // the refusal stopped happening.
        assert_eq!(
            ask(&mut runtime, "/read", &[]).await,
            "MISS",
            "the credentialed caller's answer was stored after all"
        );
    }

    /// The other half of the same decision: a call this API does not accept is
    /// still an exception, because it is a mistake in the source and is the
    /// same on every request.
    #[tokio::test]
    async fn a_call_this_api_does_not_accept_still_throws() {
        let code = r#"
            export default async function handler(context) {
                let threwForTheKey = null;
                try {
                    await caches.default.put(
                        new Request("https://example.invalid/orders", { method: "POST" }),
                        new Response("x"),
                    );
                } catch (e) { threwForTheKey = e.message; }

                let threwForTheOptions = null;
                try {
                    await caches.default.put(
                        "https://example.invalid/page",
                        new Response("x"),
                        { ignoreVary: true },
                    );
                } catch (e) { threwForTheOptions = e.message; }

                // And one that is judged rather than rejected, in the same
                // handler, so the two are told apart by this test and not by
                // two tests that could drift.
                const kept = await caches.default.put(
                    "https://example.invalid/page",
                    new Response("x", { headers: { "set-cookie": "session=alice" } }),
                );
                return new Response(JSON.stringify({
                    threwForTheKey,
                    threwForTheOptions,
                    answered: kept.stored ? null : kept.reason,
                }));
            }
        "#;
        let mut runtime = runtime("cache_throws_vs_answers.production", code).await;
        let seen = ask_json(&mut runtime, "/page", &[]).await;
        assert!(
            seen["threwForTheKey"]
                .as_str()
                .unwrap_or_default()
                .contains("whose method is POST"),
            "a key that is not a GET is a mistake in the source and has to throw: {seen}"
        );
        assert!(
            seen["threwForTheOptions"]
                .as_str()
                .unwrap_or_default()
                .contains("ignoreVary"),
            "an option this cache will not honour has to throw: {seen}"
        );
        assert!(
            seen["answered"]
                .as_str()
                .unwrap_or_default()
                .contains("set-cookie"),
            "a response this platform will not keep is answered, not thrown: {seen}"
        );
    }

    /// Code in a stretch that never said what it was answering is refused.
    ///
    /// Every runner this platform has today says something, so there is no
    /// application path here -- which is the point. The branch is the default
    /// the whole arrangement rests on: the *next* runner, written without the
    /// statement, inherits nothing and is refused rather than being handed
    /// whichever request the isolate served last. `run_unstated_stretch` is
    /// that runner, standing in for one nobody has written yet.
    #[tokio::test]
    async fn code_that_never_said_what_it_answers_is_refused() {
        let code = r#"
            globalThis.__test_outcome = null;
            globalThis.__test_put = async function() {
                const kept = await caches.default.put(
                    "https://example.invalid/unattributed",
                    new Response("from-nowhere"),
                );
                globalThis.__test_outcome = kept.stored ? "stored" : kept.reason;
            };
            export default async function handler(context) {
                return new Response(String(globalThis.__test_outcome));
            }
        "#;
        let mut runtime = runtime("cache_unattributed.production", code).await;

        runtime
            .run_unstated_stretch("globalThis.__test_put()".to_string())
            .await
            .expect("the script runs");
        let outcome = ask(&mut runtime, "/report", &[]).await;
        assert!(
            outcome.contains("nothing here says what request this code is answering"),
            "a stretch that stated nothing was allowed to cache: {outcome}"
        );
    }

    /// The statement "this code answers no request" is the glue's to make,
    /// once, and never an application's.
    ///
    /// The name is sealed, but sealing protects a name and a value, not what
    /// calling the value does -- and one call from a fetch handler would waive
    /// the disclosure rule for the rest of the stretch: a put judged with no
    /// served request reads only the key's headers, never the credentialed
    /// request actually being answered. So the function burns out on the
    /// module-evaluation call, which runs before a line of application code
    /// has, and a handler's call states nothing.
    #[tokio::test]
    async fn an_application_cannot_state_that_it_answers_no_request() {
        let code = r#"
            export default async function handler(context) {
                globalThis.__deis_runtime_answers_no_request();
                const kept = await caches.default.put(
                    "https://example.invalid/private",
                    new Response("alices-private-page"),
                );
                const back = await caches.default.match("https://example.invalid/private");
                return new Response(JSON.stringify({
                    stored: kept.stored,
                    reason: kept.reason,
                    readBack: back === undefined ? null : await back.text(),
                }));
            }
        "#;
        let mut runtime = runtime("cache_no_request_claim.production", code).await;

        // A credentialed request. The handler's call to the sealed global must
        // not stop the put being judged against it.
        let with_cookie = ask(&mut runtime, "/private", &[("cookie", "session=alice")]).await;
        let seen: serde_json::Value = serde_json::from_str(&with_cookie).expect("json");
        assert_eq!(
            seen["stored"], false,
            "a handler that called the no-request statement cached a credentialed answer: {seen}"
        );
        assert!(
            seen["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("cookie"),
            "the refusal names the header that carried the identity: {seen}"
        );
        assert_eq!(
            seen["readBack"],
            serde_json::Value::Null,
            "an entry was filed under the bare URL for the next caller: {seen}"
        );
    }

    /// A runner that answers no request says so, and may still cache.
    ///
    /// The refusal above must not become "nothing outside a request may use the
    /// cache": warming a cache from a cron tick, or from a queue batch, is the
    /// ordinary reason those runners touch it at all.
    #[tokio::test]
    async fn a_queue_batch_and_a_cron_tick_may_still_cache() {
        let code = r#"
            async function keep(what) {
                const kept = await caches.default.put(
                    "https://example.invalid/" + what,
                    new Response("warmed-by-" + what),
                );
                return kept.stored ? "stored" : kept.reason;
            }
            export async function queue(batch, env, context) {
                globalThis.__from_queue = await keep("queue");
            }
            export async function scheduled(event, env, context) {
                globalThis.__from_cron = await keep("cron");
            }
            export default async function handler(context) {
                const q = await caches.default.match("https://example.invalid/queue");
                const c = await caches.default.match("https://example.invalid/cron");
                return new Response(JSON.stringify({
                    fromQueue: globalThis.__from_queue ?? null,
                    fromCron: globalThis.__from_cron ?? null,
                    queueEntry: q === undefined ? null : await q.text(),
                    cronEntry: c === undefined ? null : await c.text(),
                }));
            }
        "#;
        let mut runtime = runtime("cache_no_request_runners.production", code).await;

        runtime
            .execute_queue_batch(
                "jobs".to_string(),
                vec![LeasedMessage {
                    id: "m1".to_string(),
                    body: b"{}".to_vec(),
                    attempts: 1,
                    lease_expires_at: 0,
                    created_at: 1_700_000_000,
                    lease_token: "lease-of-m1".to_string(),
                }],
            )
            .await
            .expect("the batch runs");
        runtime
            .execute_scheduled("* * * * *".to_string(), 1_700_000_000_000)
            .await
            .expect("the tick runs");

        let seen = ask_json(&mut runtime, "/report", &[]).await;
        assert_eq!(seen["fromQueue"], "stored", "{seen}");
        assert_eq!(seen["fromCron"], "stored", "{seen}");
        assert_eq!(seen["queueEntry"], "warmed-by-queue", "{seen}");
        assert_eq!(seen["cronEntry"], "warmed-by-cron", "{seen}");
    }

    /// The producer of a streaming body is still answering the request whose
    /// head has already gone out.
    ///
    /// It runs from the node's pump, between invocations, so before this it
    /// read whichever request the isolate had served most recently -- and
    /// between two chunks of one body that can be any number of other requests.
    /// The two callers here differ only in the credential they carry, and each
    /// one's producer has to be judged against its own.
    #[tokio::test]
    async fn a_response_stream_producer_is_still_answering_its_own_request() {
        let code = r#"
            globalThis.__marker = 0;
            globalThis.__from_producer = null;
            globalThis.__seen_marker = null;
            export default async function handler(context) {
                const path = new URL(context.request.url).pathname;
                if (path === "/report") {
                    return new Response(JSON.stringify({
                        outcome: globalThis.__from_producer,
                        markerWhenItRan: globalThis.__seen_marker,
                    }));
                }
                if (path === "/unrelated") {
                    globalThis.__marker = 1;
                    return new Response("nothing to do with that body");
                }
                globalThis.__marker = 0;
                globalThis.__from_producer = null;
                globalThis.__seen_marker = null;
                let sent = 0;
                const body = new ReadableStream({
                    async pull(controller) {
                        // Five chunks that touch nothing. The node's queue
                        // holds two, and a read pulls the next chunk before the
                        // write of the last one, so this is comfortably more
                        // than the invocation that produced the head can get
                        // through before the producer parks in a blocked write.
                        // What happens after that really is happening from the
                        // node's pump, which is what this test is about.
                        if (sent < 5) { sent++; controller.enqueue("x"); return; }
                        // Read before the await, so it says which stretch this
                        // producer ran in rather than which one it finished in.
                        globalThis.__seen_marker = globalThis.__marker;
                        const kept = await caches.default.put(
                            "https://example.invalid/streamed",
                            new Response("produced"),
                        );
                        globalThis.__from_producer = kept.stored ? "stored" : kept.reason;
                        controller.close();
                    },
                });
                return new Response(body);
            }
        "#;
        let mut runtime = runtime("cache_stream_producer.production", code).await;

        // A credentialed caller. Its producer's put is that caller's answer,
        // and nothing files it under them.
        let seen = drive_one_stream(&mut runtime, &[("cookie", "session=alice")]).await;
        assert_eq!(
            seen["markerWhenItRan"], 1,
            "this test proves nothing unless the producer really did run after the unrelated \
             request, from the node's pump: {seen}"
        );
        assert!(
            seen["outcome"]
                .as_str()
                .unwrap_or_default()
                .contains("cookie"),
            "the producer of a credentialed caller's body was not judged against that caller: \
             {seen}"
        );

        // An anonymous one, whose producer may cache. Both readings have to be
        // reachable, or the assertion above would hold on a platform that
        // refused every producer.
        let seen = drive_one_stream(&mut runtime, &[]).await;
        assert_eq!(seen["markerWhenItRan"], 1, "{seen}");
        assert_eq!(
            seen["outcome"], "stored",
            "an anonymous caller's producer may cache what it produced: {seen}"
        );
    }

    /// One streaming request, driven the way a client drives one: take the
    /// head, run something else, then start reading the body so the producer
    /// gets past the chunks the node's queue was holding.
    async fn drive_one_stream(
        runtime: &mut AppRuntime,
        headers: &[(&str, &str)],
    ) -> serde_json::Value {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        runtime
            .execute_fetch(
                "http://localhost/stream".to_string(),
                "GET".to_string(),
                headers,
                None,
                tx,
            )
            .await
            .expect("the request runs");
        let (status, _headers, body) = rx.await.expect("a head").expect("a head");
        assert_eq!(status, 200);
        let mut reader = match body {
            crate::runtime::ResponseBody::Stream(reader) => reader,
            other => panic!("the handler did not answer with a stream: {other:?}"),
        };

        // One unrelated anonymous request between the head and the body, which
        // is the whole point: the pump runs between invocations, and what a
        // producer used to read there was whichever request the isolate had
        // served most recently.
        let _ = ask(runtime, "/unrelated", &[]).await;

        // Read what the queue is holding, so the write the producer is parked
        // in can complete, and keep reading until the body ends. Reading and
        // pumping together, the way the node does it: nothing else drives this
        // producer.
        for _ in 0..24 {
            let pumping = runtime.pump_response_streams();
            let reading = std::future::poll_fn(|cx| reader.poll_chunk(cx));
            let ended = tokio::select! {
                () = pumping => true,
                chunk = reading => chunk.is_none(),
            };
            if ended {
                break;
            }
        }
        runtime.pump_response_streams().await;

        ask_json(runtime, "/report", &[]).await
    }

    /// Every way out of one request answers it.
    ///
    /// Not about the cache, and here because it is the other half of the same
    /// defect: an uncaught refusal used to kill the invocation, and the path a
    /// killed invocation takes out of `execute_fetch` returned through `?` with
    /// the channel it was given untouched. What the caller then reported was
    /// its own guess for a dropped channel -- "Runtime thread dropped" -- in
    /// place of the reason, on this path and on the two beside it.
    #[tokio::test]
    async fn an_invocation_that_was_cut_off_still_answers_the_request() {
        let code = r#"
            export default async function handler(context) {
                for (;;) {}
            }
        "#;
        let mut runtime = AppRuntime::new(
            "cache_cut_off.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec {
                limits: crate::config::ExecutionLimits {
                    timeout_ms: 200,
                    ..crate::config::ExecutionLimits::default()
                },
                ..IsolateSpec::default()
            },
            &ServiceWiring::default(),
            ServiceRouter::detached(),
        )
        .expect("the app builds");
        runtime.init_module().await.expect("the module loads");

        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = runtime
            .execute_fetch(
                "http://localhost/spin".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
                tx,
            )
            .await;
        // The channel carried something. Before this it was dropped, and the
        // caller could only say that it had been.
        let answered = rx.await.expect("the request was answered, not abandoned");
        let error = answered.expect_err("a handler that never returns cannot succeed");
        let said = error.to_string();
        assert!(
            said.contains("200ms") || said.to_lowercase().contains("time"),
            "the reason the request failed has to reach the client: {said}"
        );
    }

    /// Work handed to `waitUntil` by a stretch that has since ended does not
    /// run inside another invocation's tail.
    ///
    /// `__pending_wait_until` is the same shape of state as the request being
    /// answered: written by one invocation, read by whichever one next reaches
    /// its tail. A WebSocket message handler has no tail at all, so a
    /// `waitUntil` from one used to be awaited inside the next request -- on
    /// that request's budget, in its trace, and with its statement about the
    /// request being answered in force.
    #[tokio::test]
    async fn wait_until_work_from_a_stretch_that_ended_does_not_run_inside_another() {
        let code = r#"
            let held = null;
            globalThis.__ran = 0;
            export default async function handler(context) {
                const path = new URL(context.request.url).pathname;
                if (path === "/report") {
                    return new Response(String(globalThis.__ran));
                }
                const pair = new WebSocketPair();
                const socket = pair[1];
                held = context.waitUntil;
                socket.addEventListener("message", () => {
                    // A thenable rather than a promise, because a promise
                    // started when it was created and would run whether or not
                    // a tail ever awaited it. This runs when, and only when,
                    // something awaits it -- which is exactly the question.
                    held({ then(onFulfilled) { globalThis.__ran++; onFulfilled(); } });
                });
                return new Response(JSON.stringify({ id: socket.ws_id }));
            }
        "#;
        let mut runtime = runtime("cache_wait_until_orphan.production", code).await;

        let opened = ask_json(&mut runtime, "/open", &[]).await;
        let id = opened["id"].as_str().expect("a socket id").to_string();
        runtime
            .dispatch_ws_message(id, "go".to_string())
            .await
            .expect("the frame is delivered");

        // A later request, whose tail is the one that used to pick the orphan
        // up. Two of them, because the first request's own tail is where it
        // would have been awaited and the second proves it was not simply
        // deferred.
        let ran = ask(&mut runtime, "/report", &[]).await;
        assert_eq!(
            ran, "0",
            "a WebSocket message handler's waitUntil work was awaited inside a later request"
        );
        let ran = ask(&mut runtime, "/report", &[]).await;
        assert_eq!(ran, "0", "and it was not merely deferred by one request");
    }
}
