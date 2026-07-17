# Denix 测试文档

本项目包含完整的单元测试和集成测试。

## 测试结构

```
denix/
├── src/
│   ├── runtime/
│   │   ├── ops_test.rs          # Ops 单元测试
│   │   └── isolate_test.rs      # Runtime Isolate 单元测试
│   └── app/
│       └── manager_test.rs      # 应用管理器单元测试
└── tests/
    └── integration_test.rs      # 集成测试
```

## 运行测试

### 运行所有测试

```bash
cargo test
```

### 运行特定测试

```bash
# 运行 ops 测试
cargo test ops_test

# 运行 isolate 测试
cargo test isolate_test

# 运行 manager 测试
cargo test manager_test

# 运行集成测试
cargo test integration_test
```

### 详细输出

```bash
cargo test -- --nocapture
```

### 单个测试

```bash
cargo test test_kv_set_and_get -- --nocapture
```

## 测试覆盖

### 1. Runtime Ops 测试 (`src/runtime/ops_test.rs`)

测试 Rust ↔ JavaScript ops 通信：

- ✅ `test_kv_set_and_get` - KV 存储的设置和获取
- ✅ `test_kv_delete` - KV 删除操作
- ✅ `test_kv_list` - KV 列表操作（前缀匹配）

### 2. Runtime Isolate 测试 (`src/runtime/isolate_test.rs`)

测试 V8 Runtime 隔离层：

- ✅ `test_transpile_typescript` - TypeScript 转译
- ✅ `test_transpile_invalid_code` - 错误代码处理
- ✅ `test_create_runtime` - Runtime 创建
- ✅ `test_execute_fetch` - 执行应用 fetch 函数
- ✅ `test_execute_fetch_with_kv` - 使用 KV 存储
- ✅ `test_execute_fetch_error_handling` - 错误处理

### 3. Application Manager 测试 (`src/app/manager_test.rs`)

测试应用管理器：

- ✅ `test_load_app` - 加载应用
- ✅ `test_load_duplicate_app` - 重复加载检测
- ✅ `test_unload_app` - 卸载应用
- ✅ `test_update_app` - 更新应用
- ✅ `test_execute_app` - 执行应用
- ✅ `test_list_apps` - 列出应用
- ✅ `test_get_stats` - 统计信息
- ✅ `test_app_with_kv` - 应用使用 KV 存储

### 4. 集成测试 (`tests/integration_test.rs`)

测试完整的 HTTP 请求流程：

- ✅ `test_health_endpoint` - 健康检查端点
- ✅ `test_create_and_execute_app` - 创建并执行应用
- ✅ `test_list_apps` - 列出应用 API
- ✅ `test_update_app` - 更新应用 API
- ✅ `test_delete_app` - 删除应用 API
- ✅ `test_app_with_path` - 带路径的应用请求

## 测试示例

### 单元测试示例

```rust
#[tokio::test]
async fn test_kv_set_and_get() {
    let mut runtime = setup_test_runtime();

    // Set value
    let set_code = r#"
        (async () => {
            await Deno.core.ops.op_kv_set("test_key", "test_value");
            return "ok";
        })()
    "#;

    let result = runtime.execute_script("<test>", set_code).unwrap();
    runtime.resolve_value(result).await.unwrap();

    // Get value
    let get_code = r#"
        (async () => {
            return await Deno.core.ops.op_kv_get("test_key");
        })()
    "#;

    let result = runtime.execute_script("<test>", get_code).unwrap();
    let value = runtime.resolve_value(result).await.unwrap();

    // 验证结果
    assert_eq!(value, Some("test_value".to_string()));
}
```

### 集成测试示例

```rust
#[tokio::test]
async fn test_create_and_execute_app() {
    let app_manager = Arc::new(ApplicationManager::new());
    let app = create_test_app(app_manager).await;

    // Create app
    let create_request = Request::builder()
        .method("POST")
        .uri("/api/apps")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "id": "test_app",
                "code": r#"
                    export default {
                        fetch(request) {
                            return new Response("Integration test");
                        }
                    }
                "#
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.clone().oneshot(create_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // Execute app
    let exec_request = Request::builder()
        .uri("/apps/test_app")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(exec_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
```

## 持续集成

测试可以集成到 CI/CD 流程：

```yaml
# .github/workflows/test.yml
name: Tests

on: [push, pull_request]

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v2
      - uses: actions-rs/toolchain@v1
        with:
          toolchain: stable
      - run: cargo test --all-features
```

## 测试覆盖率

可以使用 `tarpaulin` 生成测试覆盖率报告：

```bash
# 安装 tarpaulin
cargo install cargo-tarpaulin

# 生成覆盖率报告
cargo tarpaulin --out Html --output-dir coverage
```

## 基准测试

可以添加基准测试来测试性能：

```bash
# 安装 criterion
cargo install cargo-criterion

# 运行基准测试
cargo criterion
```

## 最佳实践

1. **隔离测试**: 每个测试应该独立，不依赖其他测试
2. **清理资源**: 测试后清理创建的资源
3. **异步测试**: 使用 `#[tokio::test]` 测试异步代码
4. **错误场景**: 测试错误情况，不只是正常流程
5. **边界条件**: 测试边界值和极端情况

## 调试测试

```bash
# 显示日志输出
RUST_LOG=debug cargo test -- --nocapture

# 运行单个测试并显示输出
cargo test test_name -- --nocapture --test-threads=1
```

## 测试统计

当前测试统计：
- **单元测试**: 14 个
- **集成测试**: 6 个
- **总计**: 20 个测试

测试覆盖的模块：
- ✅ Runtime Ops
- ✅ Runtime Isolate
- ✅ Application Manager
- ✅ HTTP Gateway
- ✅ 完整请求流程

## 添加新测试

在相应模块的测试文件中添加新测试：

```rust
#[tokio::test]
async fn test_new_feature() {
    // 设置
    let manager = ApplicationManager::new();

    // 执行
    let result = manager.some_method().await;

    // 验证
    assert!(result.is_ok());
}
```

## 问题排查

### 测试失败

1. 检查错误消息
2. 使用 `--nocapture` 查看输出
3. 单独运行失败的测试
4. 检查是否有资源冲突

### 编译错误

1. 检查依赖版本
2. 运行 `cargo clean`
3. 更新依赖: `cargo update`

## 贡献

提交 PR 时请确保：
1. 所有测试通过
2. 新功能有相应的测试
3. 测试覆盖率不降低
