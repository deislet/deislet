// Database Ops unit tests

#[cfg(test)]
mod tests {
    use crate::runtime::db_ops::init_db_extension;
    use crate::runtime::ops::{init_ops_extension, ResponseStore};
    use crate::runtime::ServiceRouter;
    use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions};
    use std::cell::RefCell;
    // use std::collections::HashMap;
    use std::rc::Rc;

    fn setup_test_runtime() -> JsRuntime {
        let runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_ops_extension(), init_db_extension()],
            ..Default::default()
        });

        // Inject necessary Stores (DbClient not injected in tests to test error handling)
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        let response_store: ResponseStore = Rc::new(RefCell::new(None));
        state.put(response_store);

        runtime
    }

    #[tokio::test]
    async fn test_db_execute_without_client() {
        let mut runtime = setup_test_runtime();

        // Test error handling without DbClient
        let code = r#"
            (async () => {
                try {
                    await Deno.core.ops.op_db_execute("SELECT 1", null);
                    return "should_not_reach";
                } catch (error) {
                    return error.message;
                }
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        let resolved = runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();

        // Verify that error information is returned (via runtime internal mechanism)
        // Should throw "Database client not configured" error since DbClient is not configured
        // Actual test verifies that the code execution does not crash
        drop(resolved);
    }

    #[tokio::test]
    async fn test_db_query_without_client() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                try {
                    await Deno.core.ops.op_db_query("SELECT 1", null);
                    return "should_not_reach";
                } catch (error) {
                    return error.message;
                }
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_db_batch_without_client() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                try {
                    await Deno.core.ops.op_db_batch(JSON.stringify([
                        { sql: "INSERT INTO test VALUES (?)", params: "[1]" }
                    ]));
                    return "should_not_reach";
                } catch (error) {
                    return error.message;
                }
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_db_prepare_without_client() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            (async () => {
                try {
                    await Deno.core.ops.op_db_prepare("SELECT * FROM test WHERE id = ?");
                    return "should_not_reach";
                } catch (error) {
                    return error.message;
                }
            })()
        "#;

        let result = runtime.execute_script("<test>", code).unwrap();
        let promise_future = runtime.resolve(result);
        runtime
            .with_event_loop_promise(promise_future, PollEventLoopOptions::default())
            .await
            .unwrap();
    }

    // Note: Complete integration tests require a real deis-control server
    // These tests only verify the basic structure and error handling of ops
    // Real Database operation tests are in db_client.rs, marked as #[ignore]

    // The tests below go the whole way instead: a real isolate running
    // `js/runtime.js` against a real deis-store. Nothing less would prove
    // anything about `ctx.services.database.batch` or `.transaction`, because
    // what each promises is a property of three things at once -- the
    // JavaScript wrapper, the ops, and what deis-store does on the far side.

    use crate::runtime::isolate::{AppRuntime, IsolateSpec};
    use crate::runtime::test_store::TestStore;
    use std::collections::HashMap;
    use tokio::sync::oneshot;

    /// A table to write into, created outside any transaction.
    const CREATE_TABLE: &str = r#"await ctx.services.database.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)"
    );"#;

    /// Run one GET through a real isolate wired to `store`, and return the
    /// handler's body. The handlers below answer with JSON describing what
    /// they saw, so a failure inside one shows up as an assertion here rather
    /// than as a 500 with no detail.
    async fn database_probe(app_id: &str, store: &TestStore, body_js: &str) -> String {
        let code = format!("export default async function handler(ctx) {{\n{body_js}\n}}");
        let mut runtime = AppRuntime::new(
            app_id.to_string(),
            code,
            None,
            IsolateSpec::default(),
            &store.wiring_for(["database"]),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .expect("the isolate is built");
        runtime.init_module().await.expect("the module loads");

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
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

    // `ctx.services.database.batch` had the same problem the whole brief is
    // about: `op_db_batch` was written, registered in the extension, and
    // called by no line of JavaScript anywhere. These are the first calls it
    // has ever had from an application, so they go the whole way through.

    #[tokio::test]
    async fn a_batch_applies_every_statement_and_reports_one_result_per_statement() {
        // The case a batch exists for: take the stock and record the order,
        // with no moment in between at which one is true and the other is
        // not.
        let store = TestStore::start().await;
        let body = database_probe(
            "db_batch_commit_app.production",
            &store,
            r#"const db = ctx.services.database;
                await db.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, stock INTEGER)");
                await db.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, item INTEGER, buyer TEXT)");
                await db.execute("INSERT INTO items (id, stock) VALUES (?, ?)", [1, 5]);

                const results = await db.batch([
                    { sql: "UPDATE items SET stock = stock - 1 WHERE id = ? AND stock > 0", params: [1] },
                    { sql: "INSERT INTO orders (item, buyer) VALUES (?, ?)", params: [1, "ada"] },
                ]);

                const items = await db.query("SELECT stock FROM items WHERE id = 1");
                const orders = await db.query("SELECT item, buyer FROM orders");
                return new Response(JSON.stringify({
                    resultCount: results.length,
                    affected: results.map((r) => r.rows_affected),
                    stock: items[0].stock,
                    orders,
                }));"#,
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(
            seen["resultCount"], 2,
            "a batch answers with one result per statement: {body}"
        );
        assert_eq!(
            seen["affected"],
            serde_json::json!([1, 1]),
            "each result reports what its own statement did: {body}"
        );
        assert_eq!(seen["stock"], 4, "the stock was not taken: {body}");
        assert_eq!(
            seen["orders"],
            serde_json::json!([{ "item": 1, "buyer": "ada" }]),
            "the parameters a batch statement carried have to reach the store and bind in \
             order: {body}"
        );
    }

    #[tokio::test]
    async fn a_batch_that_fails_halfway_leaves_nothing_behind() {
        // The whole reason to use one. The first statement succeeds and the
        // second violates a constraint; if the first survived, an application
        // would have taken the stock for an order that does not exist.
        let store = TestStore::start().await;
        let body = database_probe(
            "db_batch_rollback_app.production",
            &store,
            r#"const db = ctx.services.database;
                await db.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, stock INTEGER)");
                await db.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, buyer TEXT NOT NULL)");
                await db.execute("INSERT INTO items (id, stock) VALUES (?, ?)", [1, 5]);

                let caught = null;
                try {
                    await db.batch([
                        { sql: "UPDATE items SET stock = stock - 1 WHERE id = ?", params: [1] },
                        { sql: "INSERT INTO orders (buyer) VALUES (?)", params: [null] },
                    ]);
                } catch (e) {
                    caught = e.message;
                }

                const items = await db.query("SELECT stock FROM items WHERE id = 1");
                const orders = await db.query("SELECT id FROM orders");
                return new Response(JSON.stringify({
                    caught,
                    stock: items[0].stock,
                    orderCount: orders.length,
                }));"#,
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        let caught = seen["caught"].as_str().unwrap_or_default();
        assert!(
            caught.contains("rolled back"),
            "the handler has to be told the batch was rolled back: {body}"
        );
        assert!(
            caught.contains("statement 1"),
            "and which statement did it, counted the way the array is: {body}"
        );
        assert_eq!(
            seen["stock"], 5,
            "the statement that had already succeeded must have been taken back with the rest: \
             {body}"
        );
        assert_eq!(seen["orderCount"], 0, "{body}");
    }

    #[tokio::test]
    async fn a_batch_statement_that_would_return_rows_is_refused() {
        // A batch is one round trip and nothing reads its rows, so a SELECT
        // in one could only be a mistake. deis-store refuses it, and it costs
        // the batch: this is why a read that a later statement depends on
        // belongs in `transaction`, not here.
        let store = TestStore::start().await;
        let body = database_probe(
            "db_batch_select_app.production",
            &store,
            r#"const db = ctx.services.database;
                await db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
                let caught = null;
                try {
                    await db.batch([
                        { sql: "INSERT INTO t (v) VALUES (?)", params: ["a"] },
                        { sql: "SELECT v FROM t" },
                    ]);
                } catch (e) {
                    caught = e.message;
                }
                const rows = await db.query("SELECT v FROM t");
                return new Response(JSON.stringify({ caught, rows: rows.map((r) => r.v) }));"#,
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        let caught = seen["caught"].as_str().unwrap_or_default();
        assert!(
            caught.contains("statement 1") && caught.contains("rolled back"),
            "a row-returning statement in a batch has to be named and take the batch with it: \
             {body}"
        );
        assert!(
            caught.contains("did you mean to call query"),
            "and the refusal has to point at the call that would have worked: {body}"
        );
        assert_eq!(
            seen["rows"],
            serde_json::json!([]),
            "and it takes the rest of the batch down with it: {body}"
        );
    }

    #[test]
    fn a_batch_takes_parameters_as_an_array_or_as_the_encoded_string_execute_takes() {
        // Two callers, one op. `js/runtime.js` sends the array; anything
        // holding what `op_db_execute` takes sends the JSON string. Reading
        // only one of the two is how a statement reaches the store with
        // nothing bound. An application cannot reach `Deno.core.ops`, so the
        // second form is checked here, where the decoding lives; the array
        // form is proved end to end by the batch tests above.
        use crate::runtime::db_ops::decode_batch_statements;

        let decoded = decode_batch_statements(
            r#"[
                { "sql": "INSERT INTO t (v, n) VALUES (?, ?)", "params": ["array", 1] },
                { "sql": "INSERT INTO t (v, n) VALUES (?, ?)", "params": "[\"string\", 2]" },
                { "sql": "DELETE FROM t" },
                { "sql": "DELETE FROM u", "params": null }
            ]"#,
        )
        .expect("every one of these is a legal statement");

        let params: Vec<Option<&str>> = decoded
            .iter()
            .map(|(_, params)| params.as_deref())
            .collect();
        assert_eq!(
            params,
            vec![Some(r#"["array",1]"#), Some(r#"["string", 2]"#), None, None,],
            "an array has to be encoded and an already-encoded one passed through untouched"
        );
    }

    #[test]
    fn a_batch_statement_that_could_not_run_is_named_by_its_place_in_the_array() {
        use crate::runtime::db_ops::decode_batch_statements;

        let refusal = decode_batch_statements(r#"[{ "sql": "DELETE FROM t" }, { "params": [] }]"#)
            .expect_err("a statement with no sql cannot be sent");
        let message = refusal["error"].as_str().expect("a refusal says why");
        assert!(message.contains("statement 1"), "{message}");

        let refusal = decode_batch_statements(r#"[{ "sql": "DELETE FROM t", "params": 7 }]"#)
            .expect_err("a number is not a parameter list");
        let message = refusal["error"].as_str().expect("a refusal says why");
        assert!(
            message.contains("statement 0") && message.contains("params"),
            "{message}"
        );

        let refusal = decode_batch_statements(r#"{ "sql": "DELETE FROM t" }"#)
            .expect_err("one statement is not a batch");
        assert!(
            refusal["error"]
                .as_str()
                .expect("a refusal says why")
                .contains("array"),
            "{refusal}"
        );
    }

    #[tokio::test]
    async fn a_batch_that_could_not_be_a_batch_is_refused_before_anything_is_sent() {
        // Named by index, because a batch is an array an application built in
        // a loop and "one of your statements is wrong" is not enough to find
        // the bug.
        let store = TestStore::start().await;
        let body = database_probe(
            "db_batch_refusal_app.production",
            &store,
            r#"const db = ctx.services.database;
                const caught = {};
                for (const [name, call] of [
                    ["notAnArray", () => db.batch({ sql: "SELECT 1" })],
                    ["noSql", () => db.batch([{ params: [1] }])],
                    ["badParams", () => db.batch([{ sql: "SELECT 1", params: "1" }])],
                ]) {
                    try {
                        await call();
                        caught[name] = "not refused";
                    } catch (e) {
                        caught[name] = e.constructor.name + ": " + e.message;
                    }
                }
                const empty = await db.batch([]);
                return new Response(JSON.stringify({ caught, emptyLength: empty.length }));"#,
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        for name in ["notAnArray", "noSql", "badParams"] {
            let caught = seen["caught"][name].as_str().unwrap_or_default();
            assert!(
                caught.starts_with("TypeError"),
                "{name} was not refused: {body}"
            );
        }
        assert!(
            seen["caught"]["noSql"]
                .as_str()
                .unwrap_or_default()
                .contains("statement 0"),
            "a refusal has to name the statement it is about: {body}"
        );
        assert_eq!(
            seen["emptyLength"], 0,
            "an empty batch is nothing to do, not an error: {body}"
        );
    }

    #[tokio::test]
    async fn a_transaction_commits_when_the_callback_returns_and_resolves_to_its_value() {
        // Two things at once, because they are the same promise: what the
        // callback returned is what the caller gets, and everything the
        // callback wrote is there afterwards.
        let store = TestStore::start().await;
        let body = database_probe(
            "db_tx_commit_app.production",
            &store,
            &format!(
                r#"{CREATE_TABLE}
                const db = ctx.services.database;
                const returned = await db.transaction(async (tx) => {{
                    await tx.execute("INSERT INTO t (v) VALUES (?)", ["one"]);
                    await tx.execute("INSERT INTO t (v) VALUES (?)", ["two"]);
                    const seen = await tx.query("SELECT v FROM t ORDER BY v");
                    return {{ countInside: seen.length, firstInside: seen[0].v }};
                }});
                const after = await db.query("SELECT v FROM t ORDER BY v");
                return new Response(JSON.stringify({{
                    returned,
                    after: after.map((r) => r.v),
                }}));"#
            ),
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(seen["returned"]["countInside"], 2, "{body}");
        assert_eq!(seen["returned"]["firstInside"], "one", "{body}");
        assert_eq!(
            seen["after"],
            serde_json::json!(["one", "two"]),
            "everything the callback wrote must be committed: {body}"
        );
    }

    #[tokio::test]
    async fn a_callback_that_throws_rolls_back_and_the_handler_sees_its_own_error() {
        // The error a handler catches has to be the one its own code raised.
        // Anything else and it cannot tell what went wrong, let alone branch
        // on it.
        let store = TestStore::start().await;
        let body = database_probe(
            "db_tx_throw_app.production",
            &store,
            &format!(
                r#"{CREATE_TABLE}
                const db = ctx.services.database;
                await db.execute("INSERT INTO t (v) VALUES (?)", ["before"]);
                let caught = null;
                try {{
                    await db.transaction(async (tx) => {{
                        await tx.execute("INSERT INTO t (v) VALUES (?)", ["doomed"]);
                        throw new Error("the handler's own problem");
                    }});
                }} catch (e) {{
                    caught = e.message;
                }}
                const rows = await db.query("SELECT v FROM t");
                return new Response(JSON.stringify({{
                    caught,
                    rows: rows.map((r) => r.v),
                }}));"#
            ),
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(
            seen["caught"], "the handler's own problem",
            "the callback's own error must arrive unwrapped: {body}"
        );
        assert_eq!(
            seen["rows"],
            serde_json::json!(["before"]),
            "the write inside the failed transaction must be gone, the one before it kept: {body}"
        );
    }

    #[tokio::test]
    async fn a_rollback_that_fails_does_not_replace_the_error_the_callback_raised() {
        // The rollback is made to fail by letting the transaction's deadline
        // get there first: by the time the callback throws, deis-store has
        // already ended the transaction, so the rollback is refused. The
        // handler must still see the error its own code raised.
        let store = TestStore::start().await;
        let body = database_probe(
            "db_tx_rollback_fails_app.production",
            &store,
            &format!(
                r#"{CREATE_TABLE}
                const db = ctx.services.database;
                let caught = null;
                try {{
                    await db.transaction(async (tx) => {{
                        await tx.execute("INSERT INTO t (v) VALUES (?)", ["doomed"]);
                        await new Promise((r) => setTimeout(r, 400));
                        throw new Error("the handler's own problem");
                    }}, {{ timeoutMs: 50 }});
                }} catch (e) {{
                    caught = e.message;
                }}
                const rows = await db.query("SELECT v FROM t");
                return new Response(JSON.stringify({{
                    caught,
                    rows: rows.map((r) => r.v),
                }}));"#
            ),
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(
            seen["caught"], "the handler's own problem",
            "a rollback that could not run must not stand in for the callback's error: {body}"
        );
        assert_eq!(
            seen["rows"],
            serde_json::json!([]),
            "nothing the transaction wrote may survive: {body}"
        );
    }

    #[tokio::test]
    async fn a_transaction_handle_kept_past_the_callback_is_refused() {
        // A statement on a spent handle cannot be part of the transaction any
        // more; the only thing it could do is run outside one and commit on
        // its own, which is exactly what the caller did not ask for.
        let store = TestStore::start().await;
        let body = database_probe(
            "db_tx_escaped_app.production",
            &store,
            &format!(
                r#"{CREATE_TABLE}
                const db = ctx.services.database;
                let escaped = null;
                await db.transaction(async (tx) => {{
                    escaped = tx;
                    await tx.execute("INSERT INTO t (v) VALUES (?)", ["inside"]);
                }});
                let caught = null;
                try {{
                    await escaped.execute("INSERT INTO t (v) VALUES (?)", ["late"]);
                }} catch (e) {{
                    caught = e.message;
                }}
                const rows = await db.query("SELECT v FROM t");
                return new Response(JSON.stringify({{
                    caught,
                    rows: rows.map((r) => r.v),
                }}));"#
            ),
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        let caught = seen["caught"].as_str().unwrap_or_default();
        assert!(
            caught.contains("already committed"),
            "a spent handle has to say why it is spent: {body}"
        );
        assert_eq!(
            seen["rows"],
            serde_json::json!(["inside"]),
            "the late statement must not have been written outside the transaction: {body}"
        );
    }

    #[tokio::test]
    async fn the_outer_database_handle_inside_a_callback_is_not_part_of_the_transaction() {
        // The mistake everyone makes, and the reason the doc comment on
        // `transaction` says so in as many words: `ctx.services.database` is
        // not redirected while the callback runs. A read through it goes to
        // another connection and sees the database as it was before the
        // transaction opened.
        let store = TestStore::start().await;
        let body = database_probe(
            "db_tx_outer_handle_app.production",
            &store,
            &format!(
                r#"{CREATE_TABLE}
                const db = ctx.services.database;
                const seen = await db.transaction(async (tx) => {{
                    await tx.execute("INSERT INTO t (v) VALUES (?)", ["inside"]);
                    const throughTx = await tx.query("SELECT v FROM t");
                    const throughDb = await db.query("SELECT v FROM t");
                    return {{
                        throughTx: throughTx.map((r) => r.v),
                        throughDb: throughDb.map((r) => r.v),
                    }};
                }});
                return new Response(JSON.stringify(seen));"#
            ),
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(
            seen["throughTx"],
            serde_json::json!(["inside"]),
            "a transaction reads its own uncommitted writes: {body}"
        );
        assert_eq!(
            seen["throughDb"],
            serde_json::json!([]),
            "the outer handle is a different connection and is outside the transaction: {body}"
        );
    }

    #[tokio::test]
    async fn a_transaction_that_outlives_its_deadline_says_so_where_the_handler_can_catch_it() {
        // deis-store rolls an over-running transaction back on its own,
        // because it is holding one of the tenant's few connections. What
        // matters here is that the handler is told, in words that say the
        // transaction was rolled back for exceeding its deadline -- silence,
        // or a bare "unknown handle", would send an application looking for a
        // bug it does not have.
        let store = TestStore::start().await;
        let body = database_probe(
            "db_tx_deadline_app.production",
            &store,
            &format!(
                r#"{CREATE_TABLE}
                const db = ctx.services.database;
                let caught = null;
                try {{
                    await db.transaction(async (tx) => {{
                        await tx.execute("INSERT INTO t (v) VALUES (?)", ["doomed"]);
                        await new Promise((r) => setTimeout(r, 400));
                        await tx.execute("INSERT INTO t (v) VALUES (?)", ["also-doomed"]);
                    }}, {{ timeoutMs: 50 }});
                }} catch (e) {{
                    caught = e.message;
                }}
                const rows = await db.query("SELECT v FROM t");
                return new Response(JSON.stringify({{
                    caught,
                    rows: rows.map((r) => r.v),
                }}));"#
            ),
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        let caught = seen["caught"].as_str().unwrap_or_default();
        assert!(
            caught.contains("rolled back") && caught.contains("50 ms deadline"),
            "the handler has to be told what happened and why: {body}"
        );
        assert!(
            caught.contains("nothing it wrote was applied"),
            "and what became of its writes: {body}"
        );
        assert_eq!(
            seen["rows"],
            serde_json::json!([]),
            "the store's own rollback has to have taken the write with it: {body}"
        );
    }

    #[tokio::test]
    async fn a_transaction_without_a_callback_is_refused_before_a_connection_is_pinned() {
        // An open transaction costs the tenant a connection and the file's
        // write lock. A call that could never have used one is turned away
        // here rather than opening one and leaking it until its deadline.
        let store = TestStore::start().await;
        let body = database_probe(
            "db_tx_no_callback_app.production",
            &store,
            r#"const db = ctx.services.database;
                let caught = null;
                try {
                    await db.transaction("SELECT 1");
                } catch (e) {
                    caught = e.constructor.name + ": " + e.message;
                }
                let badTimeout = null;
                try {
                    await db.transaction(async () => {}, { timeoutMs: 0 });
                } catch (e) {
                    badTimeout = e.constructor.name;
                }
                return new Response(JSON.stringify({ caught, badTimeout }));"#,
        )
        .await;

        let seen: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        let caught = seen["caught"].as_str().unwrap_or_default();
        assert!(caught.starts_with("TypeError"), "{body}");
        assert!(caught.contains("callback"), "{body}");
        assert_eq!(seen["badTimeout"], "TypeError", "{body}");
    }
}
