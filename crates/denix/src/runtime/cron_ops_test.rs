// Cron Ops 单元测试

#[cfg(test)]
mod tests {
    use crate::runtime::cron_ops::{init_cron_extension, CronEvent};
    use crate::runtime::ops::{init_ops_extension, KVStore, ResponseStore};
    use deno_core::{JsRuntime, RuntimeOptions};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    fn setup_test_runtime() -> JsRuntime {
        let runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_ops_extension(), init_cron_extension()],
            ..Default::default()
        });

        // 注入必要的 Store
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        let kv_store: KVStore = Rc::new(RefCell::new(HashMap::new()));
        let response_store: ResponseStore = Rc::new(RefCell::new(None));
        state.put(kv_store);
        state.put(response_store);

        runtime
    }

    #[test]
    fn test_cron_get_event_none() {
        let mut runtime = setup_test_runtime();

        // 测试没有 CronEvent 时返回 null
        let code = r#"
            const event = Deno.core.ops.op_cron_get_event();
            if (event === null) {
                "ok";
            } else {
                throw new Error("Expected null");
            }
        "#;

        runtime.execute_script("<test>", code).unwrap();
    }

    #[test]
    fn test_cron_get_event_with_data() {
        let mut runtime = setup_test_runtime();

        // 注入 CronEvent
        {
            let op_state = runtime.op_state();
            let mut state = op_state.borrow_mut();
            let event = CronEvent {
                scheduled_time: 1704067200000, // 2024-01-01 00:00:00
                cron: Some("0 0 * * *".to_string()),
            };
            state.put(event);
        }

        // 测试获取 CronEvent
        let code = r#"
            const event = Deno.core.ops.op_cron_get_event();
            if (event.scheduledTime === 1704067200000 && event.cron === "0 0 * * *") {
                "ok";
            } else {
                throw new Error(`Unexpected event: ${JSON.stringify(event)}`);
            }
        "#;

        runtime.execute_script("<test>", code).unwrap();
    }

    #[test]
    fn test_cron_register_valid() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            const result = Deno.core.ops.op_cron_register("0 0 * * *", "daily-task");
            if (result.success === true) {
                "ok";
            } else {
                throw new Error(`Expected success, got: ${JSON.stringify(result)}`);
            }
        "#;

        runtime.execute_script("<test>", code).unwrap();
    }

    #[test]
    fn test_cron_register_invalid() {
        let mut runtime = setup_test_runtime();

        let code = r#"
            const result = Deno.core.ops.op_cron_register("invalid", "bad-task");
            if (result.success === false && result.message.includes("Invalid cron expression")) {
                "ok";
            } else {
                throw new Error(`Expected error, got: ${JSON.stringify(result)}`);
            }
        "#;

        runtime.execute_script("<test>", code).unwrap();
    }

    #[test]
    fn test_cron_register_various_expressions() {
        let mut runtime = setup_test_runtime();

        // 测试多种 cron 表达式
        runtime
            .execute_script(
                "<test>",
                r#"
            {
                const result = Deno.core.ops.op_cron_register("0 0 * * *", "midnight");
                if (!result.success) throw new Error(`Failed: ${JSON.stringify(result)}`);
            }
            {
                const result = Deno.core.ops.op_cron_register("*/5 * * * *", "every-5-min");
                if (!result.success) throw new Error(`Failed: ${JSON.stringify(result)}`);
            }
            {
                const result = Deno.core.ops.op_cron_register("0 12 * * 1", "monday-noon");
                if (!result.success) throw new Error(`Failed: ${JSON.stringify(result)}`);
            }
            {
                const result = Deno.core.ops.op_cron_register("0 0 1 * *", "first-of-month");
                if (!result.success) throw new Error(`Failed: ${JSON.stringify(result)}`);
            }
        "#,
            )
            .unwrap();
    }
}
