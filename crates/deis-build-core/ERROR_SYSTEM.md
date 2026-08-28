# Build 错误处理系统设计

## 概述

Build 的新错误处理系统提供了清晰的分类、类型安全和丰富的错误信息，帮助开发者快速定位和解决问题。

## 错误层次结构

```
BuildError (顶层)
│
├─ ParseError (解析错误)
│  ├─ Syntax              // 语法错误
│  ├─ InvalidStructure    // 文件结构无效
│  └─ UnsupportedSyntax   // 不支持的语法特性
│
├─ ValidationError (验证错误)
│  ├─ SpecComplianceError (规范合规性)
│  │  ├─ MissingExport           // 缺少导出
│  │  ├─ PlatformDetection       // 平台检测代码
│  │  ├─ RawAccess               // context.raw 访问
│  │  ├─ PlatformApi             // 平台特定 API
│  │  ├─ ConditionalImport       // 条件导入
│  │  └─ InvalidHandlerSignature // 无效的 handler 签名
│  │
│  └─ CodeQualityWarning (代码质量)
│     ├─ Performance      // 性能警告
│     ├─ Security         // 安全警告
│     └─ Deprecated       // 弃用警告
│
├─ PlatformError (平台错误)
│  ├─ UnsupportedFeature    // 不支持的特性 (KV, Database 等)
│  ├─ IncompatibleRuntime   // 运行时不兼容
│  ├─ LimitExceeded         // 超出平台限制
│  └─ PlatformUnavailable   // 平台不可用
│
├─ CompilationError (编译错误)
│  ├─ Bundler              // 打包器错误
│  ├─ Adapter              // 适配器错误
│  ├─ Transform            // 代码转换错误
│  ├─ RouterGeneration     // 路由生成错误
│  └─ DependencyResolution // 依赖解析错误
│
└─ ConfigurationError (配置错误)
   ├─ InvalidConfig         // 无效配置
   ├─ MissingConfig         // 缺少配置
   ├─ Conflict              // 配置冲突
   └─ InvalidPlatformConfig // 平台配置无效
```

## 设计原则

### 1. 类型安全
使用 Rust 枚举替代字符串错误代码，编译时捕获错误类型错误。

**旧系统**:
```rust
ValidationError {
    code: "NO_EXPORTS".to_string(),  // 容易拼写错误
    // ...
}
```

**新系统**:
```rust
SpecComplianceError::MissingExport {
    file: "handler.ts".to_string(),
    location: SourceLocation { /* ... */ },
    expected: ExportRequirement::DefaultExport,
    suggestion: "Add: export default async function handler(...)".to_string(),
}
```

### 2. 信息丰富
每个错误类型包含完整的上下文信息。

```rust
pub struct SourceLocation {
    pub file: String,      // 文件路径
    pub line: usize,       // 行号
    pub column: usize,     // 列号
}
```

### 3. 可操作的建议
每个错误都提供具体的修复建议。

```rust
SpecComplianceError::PlatformApi {
    location: SourceLocation {
        file: "handler.ts".into(),
        line: 10,
        column: 5,
    },
    api_name: "Deno.env.get".to_string(),
    platform: "Deno".to_string(),
    suggestion: "Use context.env instead: const value = context.env.KEY".to_string(),
}
```

### 4. 分级处理
区分错误 (Error) 和警告 (Warning)。

- **Error**: 阻止编译，必须修复
- **Warning**: 不阻止编译，建议改进

## 错误代码映射

为了向后兼容和工具集成，每个错误类型都有对应的字符串代码。

| 错误类型 | 错误代码 | 说明 |
|---------|---------|------|
| **解析错误** | | |
| Syntax | `PARSE_SYNTAX` | TypeScript/JavaScript 语法错误 |
| InvalidStructure | `PARSE_INVALID_STRUCTURE` | 文件结构无效 |
| UnsupportedSyntax | `PARSE_UNSUPPORTED_SYNTAX` | 不支持的语法特性 |
| **规范合规性错误** | | |
| MissingExport | `SPEC_MISSING_EXPORT` | 缺少必需的导出 |
| PlatformDetection | `SPEC_PLATFORM_DETECTION` | 检测到平台检测代码 |
| RawAccess | `SPEC_RAW_ACCESS` | 访问 context.raw |
| PlatformApi | `SPEC_PLATFORM_API` | 使用平台特定 API |
| ConditionalImport | `SPEC_CONDITIONAL_IMPORT` | 条件导入 |
| InvalidHandlerSignature | `SPEC_INVALID_SIGNATURE` | 无效的 handler 签名 |
| **平台错误** | | |
| UnsupportedFeature | `PLATFORM_UNSUPPORTED_FEATURE` | 平台不支持的特性 |
| IncompatibleRuntime | `PLATFORM_INCOMPATIBLE_RUNTIME` | 运行时不兼容 |
| LimitExceeded | `PLATFORM_LIMIT_EXCEEDED` | 超出平台限制 |
| PlatformUnavailable | `PLATFORM_UNAVAILABLE` | 平台不可用 |
| **编译错误** | | |
| Bundler | `COMPILE_BUNDLER` | 打包器错误 |
| Adapter | `COMPILE_ADAPTER` | 适配器错误 |
| Transform | `COMPILE_TRANSFORM` | 代码转换错误 |
| RouterGeneration | `COMPILE_ROUTER` | 路由生成错误 |
| DependencyResolution | `COMPILE_DEPENDENCY` | 依赖解析错误 |

## 使用示例

### 创建错误

```rust
use deis_build_core::error_v2::*;

// 1. 规范合规性错误
let error = SpecComplianceError::PlatformDetection {
    location: SourceLocation {
        file: "functions/api/user.ts".to_string(),
        line: 15,
        column: 3,
    },
    detected_code: "typeof Deno !== 'undefined'".to_string(),
    suggestion: "Use unified context.services instead of platform detection".to_string(),
};

// 2. 平台错误
let error = PlatformError::LimitExceeded {
    platform: "cloudflare".to_string(),
    limit_type: PlatformLimitType::ScriptSize,
    limit_value: 1024 * 1024,  // 1MB
    actual_value: 2 * 1024 * 1024,  // 2MB
};

// 3. 编译错误
let error = CompilationError::Bundler {
    message: "Failed to resolve module 'react'".to_string(),
    vendor: "cloudflare".to_string(),
    cause: Some("Module not found in node_modules".to_string()),
};
```

### 错误传播

```rust
use deis_build_core::error_v2::*;

fn validate_handler(code: &str) -> Result<(), BuildError> {
    // 解析阶段
    let ast = parse_code(code).map_err(|e| {
        BuildError::Parse(ParseError::Syntax {
            location: SourceLocation { /* ... */ },
            message: e.to_string(),
            snippet: None,
        })
    })?;

    // 验证阶段
    check_exports(&ast).map_err(|e| {
        BuildError::Validation(ValidationError::SpecCompliance(
            SpecComplianceError::MissingExport { /* ... */ }
        ))
    })?;

    Ok(())
}
```

### 错误处理

```rust
match result {
    Err(BuildError::Validation(ValidationError::SpecCompliance(err))) => {
        eprintln!("Specification violation:");
        eprintln!("  Code: {}", err.error_code());
        eprintln!("  Error: {}", err);
    }
    Err(BuildError::Platform(PlatformError::UnsupportedFeature(err))) => {
        eprintln!("Platform compatibility issue:");
        eprintln!("{}", err.detailed_message());
    }
    Err(e) => {
        eprintln!("Build failed: {}", e);
    }
    Ok(_) => println!("Build succeeded"),
}
```

## 迁移指南

### 从旧错误系统迁移

#### 步骤 1: 更新 ValidationError 创建

**旧代码**:
```rust
result.add_error(ValidationError {
    file: handler_info.path.clone(),
    line: 1,
    column: 1,
    message: "Handler must have a default export".to_string(),
    code: "NO_EXPORTS".to_string(),
    severity: Severity::Error,
    suggestion: Some("Add: export default...".to_string()),
});
```

**新代码**:
```rust
use deis_build_core::error_v2::*;

let error = SpecComplianceError::MissingExport {
    file: handler_info.path.clone(),
    location: SourceLocation {
        file: handler_info.path.clone(),
        line: 1,
        column: 1,
    },
    expected: ExportRequirement::DefaultExport,
    suggestion: "Add: export default async function handler(context): Promise<Response>".to_string(),
};

return Err(BuildError::Validation(
    ValidationError::SpecCompliance(error)
));
```

#### 步骤 2: 更新错误代码引用

**旧代码**:
```rust
if error.code == "NO_EXPORTS" {
    // handle
}
```

**新代码**:
```rust
if let BuildError::Validation(ValidationError::SpecCompliance(
    SpecComplianceError::MissingExport { .. }
)) = error {
    // handle
}

// 或使用错误代码（向后兼容）
if error.error_code() == "SPEC_MISSING_EXPORT" {
    // handle
}
```

#### 步骤 3: 更新测试

**旧代码**:
```rust
assert_eq!(result.errors[0].code, "NO_EXPORTS");
```

**新代码**:
```rust
match &result {
    Err(BuildError::Validation(ValidationError::SpecCompliance(
        SpecComplianceError::MissingExport { .. }
    ))) => {
        // Test passed
    }
    _ => panic!("Expected MissingExport error"),
}
```

## 扩展指南

### 添加新的错误类型

1. **确定错误分类**: 选择正确的顶层分类（Parse/Validation/Platform/Compilation/Configuration）

2. **添加错误变体**:
```rust
pub enum SpecComplianceError {
    // ... 现有错误 ...

    /// 新的错误类型
    #[error("Description at {location}")]
    NewErrorType {
        location: SourceLocation,
        // 其他字段
        suggestion: String,
    },
}
```

3. **添加错误代码映射**:
```rust
impl SpecComplianceError {
    pub fn error_code(&self) -> &'static str {
        match self {
            // ... 现有映射 ...
            SpecComplianceError::NewErrorType { .. } => "SPEC_NEW_ERROR",
        }
    }
}
```

4. **添加测试**:
```rust
#[test]
fn test_new_error_type() {
    let error = SpecComplianceError::NewErrorType {
        location: SourceLocation { /* ... */ },
        suggestion: "Fix it this way".to_string(),
    };

    assert_eq!(error.error_code(), "SPEC_NEW_ERROR");
    assert!(error.to_string().contains("Description"));
}
```

## 最佳实践

### 1. 提供具体的建议

❌ **不好的建议**:
```rust
suggestion: "Fix your code".to_string()
```

✅ **好的建议**:
```rust
suggestion: "Replace `Deno.env.get('KEY')` with `context.env.KEY`".to_string()
```

### 2. 包含代码片段（如果可能）

```rust
ParseError::Syntax {
    location: SourceLocation { line: 10, /* ... */ },
    message: "Unexpected token".to_string(),
    snippet: Some("  typeof Deno !== 'undefined'\n  ^^^^^".to_string()),
}
```

### 3. 使用准确的错误分类

```rust
// ❌ 不要把所有错误都放到 ValidationError
BuildError::Validation(/* bundler error */)

// ✅ 使用正确的分类
BuildError::Compilation(CompilationError::Bundler { /* ... */ })
```

### 4. 提供足够的上下文

```rust
CompilationError::Adapter {
    vendor: "cloudflare".to_string(),
    message: "Failed to generate KV adapter".to_string(),
    suggestion: Some(
        "Ensure KV namespace is configured in .config.json:\n\
         \"services\": { \"kv\": { \"enabled\": true } }"
            .to_string()
    ),
}
```

## 错误输出格式

### 命令行输出示例

```
✗ Validation failed

Error: SPEC_PLATFORM_DETECTION
  Location: functions/api/users.ts:15:3
  Message: Platform detection code detected

  Detected: typeof Deno !== 'undefined'

  Suggestion: Use unified context.services instead of platform detection

  Learn more: https://github.com/deislet/spec#no-platform-detection
```

### JSON 输出示例

```json
{
  "error": {
    "type": "Validation",
    "subtype": "SpecCompliance",
    "code": "SPEC_PLATFORM_DETECTION",
    "location": {
      "file": "functions/api/users.ts",
      "line": 15,
      "column": 3
    },
    "message": "Platform detection code detected",
    "detected_code": "typeof Deno !== 'undefined'",
    "suggestion": "Use unified context.services instead",
    "severity": "error"
  }
}
```

## 参考

- [deislet/spec](https://github.com/deislet/spec) - 规范文档
- [Platform Capabilities](./src/platform_capabilities.rs) - 平台能力系统
- [Error Codes Reference](./ERROR_CODES.md) - 完整错误代码列表
