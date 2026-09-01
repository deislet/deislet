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
                    await Deno.core.ops.op_cache_set("default", "k" + i, "{}", body, 0);
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
                    await Deno.core.ops.op_cache_set("default", "k" + i, "{}", bytes("x"), 0);
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
                        "default", "big", "{}", new Uint8Array(300 * 1024), 0);
                } catch (e) {
                    outcome = e.message;
                }
                await Deno.core.ops.op_cache_set("default", "outcome", "{}", bytes(outcome), 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        let outcome = String::from_utf8(
            cache
                .entry("default", "outcome")
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
            cache.entry("default", "big").await,
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
        let (tx, rx) = tokio::sync::oneshot::channel();
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        runtime
            .execute_fetch(
                "http://localhost/account".to_string(),
                "GET".to_string(),
                headers,
                None,
                tx,
            )
            .await
            .expect("the request runs");
        let (status, _headers, body) = rx.await.expect("an answer").expect("an answer");
        let body = String::from_utf8(body.into_bytes().unwrap_or_default()).expect("text");
        assert_eq!(status, 200, "the handler failed: {body}");
        body
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
        let code = r#"
            export default async function handler(context) {
                const directive = context.request.headers.get("x-directive");
                const key = "https://example.invalid/" + directive;
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
                    refused,
                    stored: hit === undefined ? null : await hit.text(),
                }));
            }
        "#;
        let mut runtime = runtime("cache_directive_app.production", code).await;
        for directive in [
            "no-store",
            "private",
            "no-cache",
            "max-age=0",
            // A shared cache reads `s-maxage` first, so this one is stale on
            // arrival however long `max-age` says.
            "max-age=600, s-maxage=0",
            // The field-name forms. Three regular expressions scanning the
            // same string used to read `private="set-cookie"` as an ordinary
            // header value and store the response.
            "private=\"set-cookie\"",
            "no-cache=\"set-cookie\"",
            // Not the first directive, and spelled the way a real upstream
            // spells it.
            "public, max-age=600, no-store",
        ] {
            let answered = ask(&mut runtime, &[("x-directive", directive)]).await;
            let seen: serde_json::Value = serde_json::from_str(&answered).expect("json");
            println!("{directive} -> {seen}");
            assert_eq!(
                seen["stored"],
                serde_json::Value::Null,
                "a response carrying Cache-Control: {directive} was stored anyway"
            );
        }
    }
}
