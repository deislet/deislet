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
        assert!(
            outcome.starts_with("cache.put: this entry is 3072")
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
                    await caches.default.put(
                        "https://example.invalid/account",
                        new Response("page", { headers: { vary: "authorization" } })
                    );
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
                    await caches.default.put(new Request(KEY), fresh.clone());
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
                    await caches.default.put(
                        context.request,
                        new Response("page", { headers: { vary: "*" } })
                    );
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
                    await caches.default.put(key, new Response("secret", {
                        headers: { "cache-control": directive },
                    }));
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
                    await caches.default.put(
                        request,
                        new Response("answer-to:" + request.method),
                    );
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
                    await caches.default.put(key(), fresh.clone());
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
                    await caches.default.put(
                        request,
                        new Response("page-for:" + who, { headers }),
                        forCaller ? { forCaller } : undefined,
                    );
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
                    await caches.default.put(
                        context.request,
                        new Response(body, { status, headers }),
                    );
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
                await caches.default.put(
                    request,
                    new Response("dashboard-for:" + who, { headers }),
                );
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
                    await caches.default.put(request, new Response("page", { headers }));
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
                    await caches.default.put(request, new Response("page", { headers }));
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
                    await caches.default.put(
                        KEY,
                        new Response("dashboard-for:" + (caller ?? "nobody")),
                        options,
                    );
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
                    await caches.default.put(
                        KEY,
                        new Response("shared-config"),
                        { forAnyCaller: true },
                    );
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
