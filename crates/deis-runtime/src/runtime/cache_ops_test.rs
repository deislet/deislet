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
                    "test_key", "{\"status\":200}", bytes("test_value"), 0);
            })()
        "#;

        run(&mut runtime, code).await;

        // Verify by directly accessing CacheStore
        let cache = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<CacheStore>().clone()
        };

        let value = cache.get("test_key").await.expect("the entry was written");
        assert_eq!(value.meta, "{\"status\":200}");
        assert_eq!(value.body, b"test_value".to_vec());
    }

    #[tokio::test]
    async fn test_cache_get_nonexistent() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_get("nonexistent_key");
            })()
        "#;

        run(&mut runtime, code).await;

        let cache = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<CacheStore>().clone()
        };

        let value = cache.get("nonexistent_key").await;
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn test_cache_delete() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set("key_to_delete", "{}", bytes("value"), 0);
                await Deno.core.ops.op_cache_delete("key_to_delete");
            })()
        "#;

        run(&mut runtime, code).await;

        let cache = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<CacheStore>().clone()
        };

        let value = cache.get("key_to_delete").await;
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn test_cache_has() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set("exists_key", "{}", bytes("value"), 0);
            })()
        "#;

        run(&mut runtime, code).await;

        let cache = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<CacheStore>().clone()
        };

        let has_exists = cache.contains_key("exists_key");
        let has_not_exists = cache.contains_key("not_exists_key");

        assert!(has_exists);
        assert!(!has_not_exists);
    }

    #[tokio::test]
    async fn test_cache_clear() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set("key1", "{}", bytes("value1"), 0);
                await Deno.core.ops.op_cache_set("key2", "{}", bytes("value2"), 0);
                await Deno.core.ops.op_cache_clear();
            })()
        "#;

        run(&mut runtime, code).await;

        let cache = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<CacheStore>().clone()
        };

        cache.run_pending_tasks().await;
        let count = cache.entry_count();

        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_cache_stats() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set("stat_key1", "{}", bytes("value1"), 0);
                await Deno.core.ops.op_cache_set("stat_key2", "{}", bytes("value2"), 0);
                await Deno.core.ops.op_cache_set("stat_key3", "{}", bytes("value3"), 0);
            })()
        "#;

        run(&mut runtime, code).await;

        let cache = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<CacheStore>().clone()
        };

        cache.run_pending_tasks().await;
        let count = cache.entry_count();

        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_cache_multiple_operations() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                await Deno.core.ops.op_cache_set("multi1", "{}", bytes("value1"), 0);
                await Deno.core.ops.op_cache_set("multi2", "{}", bytes("value2"), 0);
                await Deno.core.ops.op_cache_set("multi3", "{}", bytes("value3"), 0);
                await Deno.core.ops.op_cache_delete("multi2");
            })()
        "#;

        run(&mut runtime, code).await;

        let cache = {
            let op_state = runtime.op_state();
            let state = op_state.borrow();
            state.borrow::<CacheStore>().clone()
        };

        let v1 = cache.get("multi1").await;
        let v2 = cache.get("multi2").await;
        let v3 = cache.get("multi3").await;

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
                await Deno.core.ops.op_cache_set("with_ttl", "{}", bytes("value"), 120);
                await Deno.core.ops.op_cache_set("without_ttl", "{}", bytes("value"), 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert_eq!(
            cache.get("with_ttl").await.unwrap().ttl,
            Some(Duration::from_secs(120))
        );
        // Zero is not "expire immediately"; it is "the response named no
        // lifetime", and the store's own default applies.
        assert_eq!(cache.get("without_ttl").await.unwrap().ttl, None);
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
                await Deno.core.ops.op_cache_set("present", "{}", bytes("value"), 0);
                const removedPresent = await Deno.core.ops.op_cache_delete("present");
                const removedAbsent = await Deno.core.ops.op_cache_delete("never_written");
                // Parked in the cache itself rather than read back through a
                // v8 scope: the answer under test is a value the op returned.
                await Deno.core.ops.op_cache_set(
                    "answers", "{}", bytes(removedPresent + "," + removedAbsent), 0);
            })()
        "#,
        )
        .await;

        let cache = store(&runtime);
        assert_eq!(
            cache.get("answers").await.unwrap().body,
            b"true,false".to_vec(),
            "delete() must report a hit as true and a miss as false"
        );
        assert!(cache.get("present").await.is_none());
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
        use crate::runtime::ServiceWiring;
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
}
