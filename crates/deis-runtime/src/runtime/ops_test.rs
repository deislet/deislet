// Runtime Ops unit tests

#[cfg(test)]
mod tests {
    use crate::runtime::ops::{init_ops_extension, EnvStore, ResponseStore};
    use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    fn setup_test_runtime() -> JsRuntime {
        let runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_ops_extension()],
            ..Default::default()
        });

        // Inject Response Store and Env Store
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        let response_store: ResponseStore = Rc::new(RefCell::new(None));
        let env_store: EnvStore = Rc::new(RefCell::new(HashMap::from([(
            "ENV_KEY".to_string(),
            "env_value".to_string(),
        )])));
        state.put(response_store);
        state.put(env_store);

        runtime
    }

    #[tokio::test]
    async fn test_get_env() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                const env = Deno.core.ops.op_deis_get_env();
                Deno.core.ops.op_set_response(JSON.stringify(env));
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();

        let op_state = runtime.op_state();
        let state = op_state.borrow();
        let store = state.borrow::<ResponseStore>().clone();
        let response = store.borrow().clone().unwrap();
        let env: HashMap<String, String> = serde_json::from_str(&response).unwrap();

        assert_eq!(env.get("ENV_KEY"), Some(&"env_value".to_string()));
    }

    // `ctx.services.kv.list` is a property of three things at once -- the
    // JavaScript wrapper, `op_kv_list`, and the `List` RPC deis-store answers
    // -- and it was reachable from none of them until now: the op was
    // registered and no line of JavaScript called it. A test that mocked the
    // store would prove only that the wrapper calls the op, which was never
    // the part in doubt. So these run a real isolate against a real store.

    use crate::runtime::isolate::{AppRuntime, IsolateSpec};
    use crate::runtime::test_store::TestStore;
    use crate::runtime::ServiceRouter;
    use tokio::sync::oneshot;

    /// Run one GET through a real isolate wired to `store`'s KV, and return
    /// the handler's body. The handlers below answer with JSON describing
    /// what they saw, so a failure inside one shows up as an assertion here
    /// rather than as a 500 with nothing in it.
    async fn kv_probe(app_id: &str, store: &TestStore, body_js: &str) -> String {
        let code = format!("export default async function handler(ctx) {{\n{body_js}\n}}");
        let mut runtime = AppRuntime::new(
            app_id.to_string(),
            code,
            None,
            IsolateSpec::default(),
            &store.kv_wiring_for(app_id),
            // No pool behind this isolate, so every service call it makes is
            // refused as "not deployed on this node" -- which is what it is.
            ServiceRouter::detached(),
        )
        .expect("the isolate is built");
        runtime.init_module().await.expect("the module loads");

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
                tx,
            )
            .await
            .expect("the request runs");
        let (status, _, body) = rx.await.expect("a response").expect("a response");
        let body = body
            .into_bytes()
            .map(|bytes| String::from_utf8(bytes).expect("these handlers answer with JSON"))
            .unwrap_or_default();
        assert_eq!(status, 200, "{body}");
        body
    }

    /// The same probe, but with named bindings declared beside the unnamed
    /// namespace, so a handler can reach `ctx.bindings.HOT` as well as
    /// `ctx.services.kv`.
    async fn binding_probe(
        app_id: &str,
        store: &TestStore,
        bindings: &[&str],
        body_js: &str,
    ) -> String {
        let code = format!("export default async function handler(ctx) {{\n{body_js}\n}}");
        let wiring = store.wiring_with_bindings_for(
            app_id,
            ["kv"],
            deis_bundle::BindingKind::Kv,
            bindings.to_vec(),
        );
        let mut runtime = AppRuntime::new(
            app_id.to_string(),
            code,
            None,
            IsolateSpec::default(),
            &wiring,
            ServiceRouter::detached(),
        )
        .expect("the isolate is built");
        runtime.init_module().await.expect("the module loads");

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
                tx,
            )
            .await
            .expect("the request runs");
        let (status, _, body) = rx.await.expect("a response").expect("a response");
        let body = body
            .into_bytes()
            .map(|bytes| String::from_utf8(bytes).expect("these handlers answer with JSON"))
            .unwrap_or_default();
        assert_eq!(status, 200, "{body}");
        body
    }

    #[tokio::test]
    async fn two_named_bindings_of_one_application_never_see_each_other() {
        // The property the whole mechanism rests on, checked against a real
        // deis-store rather than against the composition function: the same
        // key written through two bindings of one application is two values,
        // and neither is the unnamed namespace's.
        //
        // The names are chosen to be the nastiest pair the encoding has to
        // survive: one is a prefix of the other, so an encoding that joined
        // the name and the key without a separator the name cannot contain
        // would put `HOT` + `2:k` and `HOT2` + `:k` in one place.
        let store = TestStore::start().await;
        let body = binding_probe(
            "kv_two_bindings_app.production",
            &store,
            &["HOT", "HOT2"],
            r#"await ctx.services.kv.put("k", "unnamed");
                await ctx.bindings.HOT.put("k", "hot");
                await ctx.bindings.HOT2.put("k", "hot2");
                await ctx.bindings.HOT.put("2:k", "hot-with-a-colliding-key");
                return new Response(JSON.stringify({
                    unnamed: await ctx.services.kv.get("k"),
                    hot: await ctx.bindings.HOT.get("k"),
                    hot2: await ctx.bindings.HOT2.get("k"),
                    collider: await ctx.bindings.HOT.get("2:k"),
                    hotKeys: (await ctx.bindings.HOT.list()).keys.map((e) => e.name),
                    hot2Keys: (await ctx.bindings.HOT2.list()).keys.map((e) => e.name),
                    unnamedKeys: (await ctx.services.kv.list()).keys.map((e) => e.name),
                    envIsTheSameObject: ctx.env.HOT === ctx.bindings.HOT,
                }));"#,
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(seen["unnamed"], "unnamed", "{body}");
        assert_eq!(seen["hot"], "hot", "{body}");
        assert_eq!(seen["hot2"], "hot2", "{body}");
        assert_eq!(seen["collider"], "hot-with-a-colliding-key", "{body}");
        // Each namespace enumerates only its own keys. This is the assertion
        // that would fail if two bindings shared a namespace: HOT2 would see
        // three keys, or HOT would see HOT2's.
        assert_eq!(seen["hotKeys"], serde_json::json!(["2:k", "k"]), "{body}");
        assert_eq!(seen["hot2Keys"], serde_json::json!(["k"]), "{body}");
        assert_eq!(seen["unnamedKeys"], serde_json::json!(["k"]), "{body}");
        assert_eq!(
            seen["envIsTheSameObject"], true,
            "a binding is published on env and on context.bindings, and it is one object: {body}"
        );
    }

    #[tokio::test]
    async fn an_application_deployed_before_bindings_existed_keeps_its_data() {
        // The other half of the promise. An application writes through
        // `ctx.services.kv` -- which is what it had before any of this
        // existed -- and then a later version declares a binding. The old data
        // must still be exactly where it was, and the new namespace must start
        // empty.
        let store = TestStore::start().await;

        let before = kv_probe(
            "kv_upgrade_app.production",
            &store,
            r#"await ctx.services.kv.put("legacy", "written-before-bindings");
                return new Response(JSON.stringify({
                    value: await ctx.services.kv.get("legacy"),
                }));"#,
        )
        .await;
        let before: serde_json::Value = serde_json::from_str(&before).expect("a JSON body");
        assert_eq!(before["value"], "written-before-bindings");

        let after = binding_probe(
            "kv_upgrade_app.production",
            &store,
            &["HOT"],
            r#"return new Response(JSON.stringify({
                    unnamed: await ctx.services.kv.get("legacy"),
                    hot: await ctx.bindings.HOT.get("legacy"),
                    hotKeys: (await ctx.bindings.HOT.list()).keys.length,
                }));"#,
        )
        .await;
        let after: serde_json::Value = serde_json::from_str(&after).expect("a JSON body");
        assert_eq!(
            after["unnamed"], "written-before-bindings",
            "declaring a binding must not move what the unnamed namespace holds"
        );
        assert_eq!(
            after["hot"],
            serde_json::Value::Null,
            "a new binding is a new namespace and starts empty"
        );
        assert_eq!(after["hotKeys"], 0);
    }

    #[tokio::test]
    async fn a_name_the_manifest_did_not_declare_is_not_reachable() {
        // A binding is reachable by the name its manifest wrote down and by no
        // other route: there is no way to name a namespace at the call site,
        // which is what keeps "what can this application reach" answerable out
        // of the manifest rather than out of the code.
        let store = TestStore::start().await;
        let body = binding_probe(
            "kv_undeclared_app.production",
            &store,
            &["HOT"],
            r#"return new Response(JSON.stringify({
                    declared: Object.keys(ctx.bindings),
                    cold: ctx.bindings.COLD === undefined,
                    envCold: ctx.env.COLD === undefined,
                }));"#,
        )
        .await;
        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(seen["declared"], serde_json::json!(["HOT"]), "{body}");
        assert_eq!(seen["cold"], true, "{body}");
        assert_eq!(seen["envCold"], true, "{body}");
    }

    #[tokio::test]
    async fn a_listing_reports_each_key_with_what_was_stored_beside_it() {
        // The shape an application written for this platform expects:
        // `keys` of `{ name, expiration, metadata }`, a `list_complete` flag,
        // and the metadata decoded exactly the way `getWithMetadata` decodes
        // it -- an object goes in with put(), the same object comes out of a
        // listing.
        let store = TestStore::start().await;
        let body = kv_probe(
            "kv_list_shape_app.production",
            &store,
            r#"const kv = ctx.services.kv;
                await kv.put("session:a", "one", { metadata: { user: 7 } });
                await kv.put("session:b", "two", { expirationTtl: 3600 });
                const page = await kv.list({ prefix: "session:" });
                return new Response(JSON.stringify({
                    names: page.keys.map((k) => k.name),
                    metadata: page.keys.map((k) => k.metadata),
                    hasExpiration: page.keys.map((k) => k.expiration !== null),
                    list_complete: page.list_complete,
                    cursor: page.cursor === undefined ? "absent" : page.cursor,
                }));"#,
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(
            seen["names"],
            serde_json::json!(["session:a", "session:b"]),
            "a listing is in key order: {body}"
        );
        assert_eq!(
            seen["metadata"],
            serde_json::json!([{ "user": 7 }, null]),
            "metadata has to come back as the object put() was handed: {body}"
        );
        assert_eq!(
            seen["hasExpiration"],
            serde_json::json!([false, true]),
            "a key written with a lifetime reports when it lapses: {body}"
        );
        assert_eq!(
            seen["list_complete"], true,
            "a listing that reached the end says so: {body}"
        );
        assert_eq!(
            seen["cursor"], "absent",
            "a finished listing hands back no cursor to follow: {body}"
        );
    }

    #[tokio::test]
    async fn a_listing_sees_only_the_keys_under_its_prefix_and_only_its_own_tenant() {
        // Without a prefix a listing is the whole application, which is the
        // widest it can ever be: the tenant is the boundary, and there is no
        // argument that reaches past it.
        let store = TestStore::start().await;
        let body = kv_probe(
            "kv_list_prefix_app.production",
            &store,
            r#"const kv = ctx.services.kv;
                await kv.put("user:1", "a");
                await kv.put("user:2", "b");
                await kv.put("order:1", "c");
                const users = await kv.list("user:");
                const everything = await kv.list();
                return new Response(JSON.stringify({
                    users: users.keys.map((k) => k.name),
                    everything: everything.keys.map((k) => k.name),
                }));"#,
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(
            seen["users"],
            serde_json::json!(["user:1", "user:2"]),
            "a prefix must not pull in a neighbouring one: {body}"
        );
        assert_eq!(
            seen["everything"],
            serde_json::json!(["order:1", "user:1", "user:2"]),
            "no prefix lists everything this application has: {body}"
        );
    }

    #[tokio::test]
    async fn a_listing_pages_through_a_cursor_until_it_reports_itself_complete() {
        // The reason `list_complete` exists rather than "was the page
        // short?": deis-store answers with a cursor whenever it filled a page
        // to the limit, so five keys taken two at a time give pages of 2, 2
        // and 1, and four keys taken two at a time give 2, 2 and then an
        // empty page. Both loops have to terminate with everything collected,
        // and only the flag makes that true.
        let store = TestStore::start().await;
        let body = kv_probe(
            "kv_list_paging_app.production",
            &store,
            r#"const kv = ctx.services.kv;
                for (const n of ["1", "2", "3", "4"]) await kv.put("k:" + n, n);

                async function drain(limit) {
                    const names = [];
                    const sizes = [];
                    let page = { list_complete: false, cursor: undefined };
                    let guard = 0;
                    while (!page.list_complete) {
                        if (++guard > 10) throw new Error("the loop did not terminate");
                        page = await kv.list({ prefix: "k:", limit, cursor: page.cursor });
                        names.push(...page.keys.map((k) => k.name));
                        sizes.push(page.keys.length);
                    }
                    return { names, sizes };
                }

                const byTwo = await drain(2);
                const byThree = await drain(3);
                return new Response(JSON.stringify({ byTwo, byThree }));"#,
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        let all = serde_json::json!(["k:1", "k:2", "k:3", "k:4"]);
        assert_eq!(
            seen["byTwo"]["names"], all,
            "paging must not drop or repeat a key: {body}"
        );
        assert_eq!(
            seen["byTwo"]["sizes"],
            serde_json::json!([2, 2, 0]),
            "four keys two at a time end on an empty page, which is why a short page is not \
             the end signal: {body}"
        );
        assert_eq!(
            seen["byThree"]["names"], all,
            "paging must not drop or repeat a key: {body}"
        );
        assert_eq!(
            seen["byThree"]["sizes"],
            serde_json::json!([3, 1]),
            "a page that came back short is the last one: {body}"
        );
    }

    #[tokio::test]
    async fn a_listing_argument_that_could_not_work_is_refused_before_the_store_is_asked() {
        // A limit of zero or half a key is a caller bug. deis-store would
        // quietly substitute its default page size for it, so the application
        // would get a thousand keys back and no hint that the number it
        // passed meant nothing.
        let store = TestStore::start().await;
        let body = kv_probe(
            "kv_list_refusal_app.production",
            &store,
            r#"const kv = ctx.services.kv;
                const caught = {};
                for (const [name, call] of [
                    ["zero", () => kv.list({ limit: 0 })],
                    ["fraction", () => kv.list({ limit: 1.5 })],
                    ["prefix", () => kv.list({ prefix: 7 })],
                    ["cursor", () => kv.list({ cursor: 7 })],
                ]) {
                    try {
                        await call();
                        caught[name] = "not refused";
                    } catch (e) {
                        caught[name] = e.constructor.name;
                    }
                }
                // A limit past what the op's argument can hold is not a
                // caller bug, it is just more than the store will give: it
                // has to answer, not refuse.
                await kv.put("k", "v");
                const absurd = await kv.list({ limit: 1e12 });
                caught.absurd = absurd.keys.length === 1 ? "listed" : "wrong page";
                return new Response(JSON.stringify(caught));"#,
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        for name in ["zero", "fraction", "prefix", "cursor"] {
            assert_eq!(seen[name], "TypeError", "{name} was not refused: {body}");
        }
        assert_eq!(
            seen["absurd"], "listed",
            "a limit larger than the wire can hold is the store's ceiling, not an error: {body}"
        );
    }

    #[tokio::test]
    async fn a_listing_by_an_app_without_kv_says_the_service_is_not_there() {
        // Every other KV call says this; a `list` that answered with an empty
        // page instead would read as "you have stored nothing", which is the
        // one answer an application must not be given when the truth is that
        // it has no store at all.
        let store = TestStore::start().await;
        let mut runtime = AppRuntime::new(
            "kv_list_undeclared_app.production".to_string(),
            r#"export default async function handler(ctx) {
                try {
                    await ctx.services.kv.list("anything");
                    return new Response("listed");
                } catch (e) {
                    return new Response(e.message);
                }
            }"#
            .to_string(),
            None,
            IsolateSpec::default(),
            // The store is running and reachable; this application simply
            // never declared that it uses KV.
            &store.wiring_for_tenant("kv_list_undeclared_app.production", ["database"]),
            // No pool behind this isolate, so every service call it makes is
            // refused as "not deployed on this node" -- which is what it is.
            ServiceRouter::detached(),
        )
        .expect("the isolate is built");
        runtime.init_module().await.expect("the module loads");

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
                tx,
            )
            .await
            .expect("the request runs");
        let (_, _, body) = rx.await.expect("a response").expect("a response");
        let body = String::from_utf8(body.into_bytes().unwrap_or_default()).expect("text");
        assert!(
            body.contains("kv service is not available"),
            "an undeclared service must say so rather than list nothing: {body}"
        );
    }

    /// The fan-out allowance: what it counts and what it refuses.
    ///
    /// The charge has to be taken and committed with no await between the two,
    /// which is why this is a plain counter rather than a check followed by a
    /// commit: two hundred `fetch` calls started at once would all pass a
    /// check made before the destination was resolved and committed after it,
    /// and the ceiling would bound nothing in exactly the case it exists for.
    #[test]
    fn a_subrequest_allowance_is_spent_once_per_call_and_refunded_when_nothing_left_the_isolate() {
        use crate::runtime::ops::SubrequestAllowance;

        let allowance = SubrequestAllowance::new(3);
        assert_eq!(allowance.charge(), Ok(1), "the first call of an invocation");
        assert_eq!(allowance.charge(), Ok(2));
        assert_eq!(allowance.charge(), Ok(3));
        assert_eq!(
            allowance.charge(),
            Err(3),
            "the refusal reports the ceiling, which is what the message has to name"
        );

        // A destination the policy refused, or one abandoned before a socket
        // was opened, never left the isolate and gives its charge back.
        allowance.refund();
        assert_eq!(allowance.charge(), Ok(3));
    }

    /// A refund with no charge behind it must not hand out an unbounded
    /// allowance.
    ///
    /// It cannot happen through the two call sites as they are written, which
    /// is exactly why the saturation is worth pinning: a third caller added
    /// later that refunds on a path it did not charge on would otherwise turn
    /// the ceiling off, and nothing would report it.
    #[test]
    fn an_unmatched_refund_cannot_wrap_the_counter_round() {
        use crate::runtime::ops::SubrequestAllowance;

        let allowance = SubrequestAllowance::new(1);
        allowance.refund();
        allowance.refund();
        assert_eq!(allowance.charge(), Ok(1));
        assert_eq!(allowance.charge(), Err(1));
    }

    /// Each invocation gets an allowance of its own, and the one an earlier
    /// invocation was spending is left exactly where it was.
    ///
    /// This is the whole of the ownership rule, at the level it is decided.
    /// The budget used to be one counter for the isolate, reset at the start of
    /// every invocation -- so a response stream still holding it, which is the
    /// one thing allowed to spend past the request it belongs to, had its
    /// ceiling handed back to it by every unrelated request that arrived.
    #[test]
    fn a_new_invocation_gets_its_own_allowance_and_does_not_refill_the_last_one() {
        use crate::runtime::ops::SubrequestBudget;

        let budget = SubrequestBudget::new(3);

        // One invocation, which spends two of its three and then leaves a
        // response stream producing -- the stream keeps this allowance.
        budget.begin_invocation();
        let streaming = budget.invocation();
        assert_eq!(streaming.charge(), Ok(1));
        assert_eq!(streaming.charge(), Ok(2));

        // Ordinary traffic for the same application: a second invocation, with
        // an allowance of its own.
        budget.begin_invocation();
        let next = budget.invocation();
        assert!(
            !Rc::ptr_eq(&streaming, &next),
            "a new invocation must not be handed the allowance the last one was spending"
        );
        assert_eq!(next.charge(), Ok(1), "the new invocation starts from zero");

        // And the stream's is untouched: one call left, then refusals.
        assert_eq!(
            streaming.spent(),
            2,
            "the stream's allowance was refilled by somebody else's request"
        );
        assert_eq!(streaming.charge(), Ok(3));
        assert_eq!(streaming.charge(), Err(3));
    }

    /// The concurrent-connection ceiling refuses at its number and gives
    /// nothing back it never took. Unlike the subrequest allowance, which
    /// resets per invocation, this counter is the isolate's own -- it counts
    /// sockets open *now*, across invocations, and only a release fills it.
    #[test]
    fn the_outbound_connection_ceiling_refuses_at_its_number_and_saturates_on_release() {
        use crate::runtime::ops::OutboundConnections;

        let connections = OutboundConnections::new(2);

        assert_eq!(connections.charge(), Ok(()));
        assert_eq!(connections.charge(), Ok(()));
        assert_eq!(
            connections.charge(),
            Err(2),
            "the ceiling names itself in the refusal, the way the log line quotes it"
        );

        // One gives way; exactly one more is let in.
        connections.release();
        assert_eq!(connections.charge(), Ok(()));
        assert_eq!(connections.charge(), Err(2));

        // A wrapper that released twice -- or a bookkeeping path that did --
        // cannot turn the counter into an unbounded allowance.
        connections.release();
        connections.release();
        connections.release();
        assert_eq!(connections.open(), 0);
        assert_eq!(connections.charge(), Ok(()));
        assert_eq!(connections.charge(), Ok(()));
        assert_eq!(connections.charge(), Err(2));
    }

    /// Which allowance a call is charged to is decided by the scope it names,
    /// and a scope naming a stream this isolate no longer has falls back to
    /// the invocation's rather than to nothing.
    #[test]
    fn a_call_is_charged_to_the_allowance_its_scope_names() {
        use crate::runtime::ops::{subrequest_allowance, SubrequestBudget};
        use crate::runtime::response_stream::{response_streams, StreamOwner};
        use deno_core::{JsRuntime, RuntimeOptions};
        use std::time::{Duration, Instant};

        // A bare runtime, only for its `OpState`: this is about the lookup,
        // not about anything an application does.
        let runtime = JsRuntime::new(RuntimeOptions::default());
        let op_state = runtime.op_state();

        let budget = Rc::new(SubrequestBudget::new(5));
        op_state.borrow_mut().put(budget.clone());

        budget.begin_invocation();
        let of_the_stream = budget.invocation();

        // A response stream opened by that invocation, holding its allowance.
        let stream_id = {
            let streams = response_streams(&mut op_state.borrow_mut());
            let (id, _reader) = streams.borrow_mut().open(
                Instant::now() + Duration::from_secs(60),
                StreamOwner {
                    tenant: "scope_lookup.production".to_string(),
                    trace_id: "trace".to_string(),
                    target: "http://localhost/feed".to_string(),
                },
                of_the_stream.clone(),
            );
            id
        };

        // The next request for this application.
        budget.begin_invocation();
        let of_the_invocation = budget.invocation();

        let state = op_state.borrow();
        assert!(
            Rc::ptr_eq(
                &subrequest_allowance(&state, stream_id).expect("a budget is installed"),
                &of_the_stream
            ),
            "a producer's call has to come out of the allowance its own invocation had"
        );
        assert!(
            Rc::ptr_eq(
                &subrequest_allowance(&state, 0).expect("a budget is installed"),
                &of_the_invocation
            ),
            "scope 0 is the invocation running now"
        );
        assert!(
            Rc::ptr_eq(
                &subrequest_allowance(&state, stream_id + 1).expect("a budget is installed"),
                &of_the_invocation
            ),
            "a scope naming no open stream falls back to the invocation's allowance, which is \
             the one that is always bounded"
        );
    }
}
