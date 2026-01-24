/// Deforge Error Classification System
///
/// 设计原则：
/// 1. 层次清晰：按错误类型分层（解析、验证、平台、编译、配置）
/// 2. 类型安全：使用枚举而非字符串代码
/// 3. 信息丰富：提供详细错误信息、位置、建议
/// 4. 可扩展：易于添加新的错误类型

use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

// ============================================================================
// 顶层错误类型
// ============================================================================

#[derive(Error, Debug)]
pub enum DeforgeError {
    /// 解析错误 - 代码结构、语法问题
    #[error("Parse error: {0}")]
    Parse(#[from] ParseError),

    /// 验证错误 - 规范合规性检查
    #[error("Validation error: {0}")]
    Validation(#[from] ValidationError),

    /// 平台错误 - 平台能力、兼容性问题
    #[error("Platform error: {0}")]
    Platform(#[from] PlatformError),

    /// 编译错误 - 转换、打包、适配器问题
    #[error("Compilation error: {0}")]
    Compilation(#[from] CompilationError),

    /// 配置错误 - 项目配置问题
    #[error("Configuration error: {0}")]
    Configuration(#[from] ConfigurationError),

    /// IO 错误
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON 序列化错误
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

// ============================================================================
// 1. 解析错误 (Parse Errors)
// ============================================================================

#[derive(Error, Debug, Clone)]
pub enum ParseError {
    /// TypeScript/JavaScript 语法错误
    #[error("Syntax error at {location}: {message}")]
    Syntax {
        location: SourceLocation,
        message: String,
        snippet: Option<String>,
    },

    /// 无效的文件结构
    #[error("Invalid file structure in {file}: {message}")]
    InvalidStructure {
        file: String,
        message: String,
    },

    /// 不支持的语法特性
    #[error("Unsupported syntax at {location}: {feature}")]
    UnsupportedSyntax {
        location: SourceLocation,
        feature: String,
        suggestion: Option<String>,
    },
}

// ============================================================================
// 2. 验证错误 (Validation Errors)
// ============================================================================

#[derive(Error, Debug, Clone)]
pub enum ValidationError {
    /// 规范合规性错误
    #[error("{0}")]
    SpecCompliance(#[from] SpecComplianceError),

    /// 代码质量警告
    #[error("{0}")]
    CodeQuality(#[from] CodeQualityWarning),
}

/// 规范合规性错误 - 违反 deislet/spec 标准
#[derive(Error, Debug, Clone)]
pub enum SpecComplianceError {
    /// 缺少必需的导出
    #[error("Missing required export in {file}")]
    MissingExport {
        file: String,
        location: SourceLocation,
        expected: ExportRequirement,
        suggestion: String,
    },

    /// 平台检测代码（禁止 typeof Deno 等）
    #[error("Platform detection code at {location}")]
    PlatformDetection {
        location: SourceLocation,
        detected_code: String,
        suggestion: String,
    },

    /// 访问 context.raw（禁止）
    #[error("Direct access to context.raw at {location}")]
    RawAccess {
        location: SourceLocation,
        accessed_property: String,
        suggestion: String,
    },

    /// 平台特定 API 使用（禁止 Deno.env, process.env 等）
    #[error("Platform-specific API usage at {location}")]
    PlatformApi {
        location: SourceLocation,
        api_name: String,
        platform: String,
        suggestion: String,
    },

    /// 条件导入（禁止基于平台的动态导入）
    #[error("Conditional import at {location}")]
    ConditionalImport {
        location: SourceLocation,
        condition: String,
        suggestion: String,
    },

    /// 无效的 Handler 签名
    #[error("Invalid handler signature at {location}")]
    InvalidHandlerSignature {
        location: SourceLocation,
        found: String,
        expected: String,
    },
}

/// 代码质量警告 - 不阻止编译但建议改进
#[derive(Error, Debug, Clone)]
pub enum CodeQualityWarning {
    /// 性能警告
    #[error("Performance concern at {location}: {message}")]
    Performance {
        location: SourceLocation,
        message: String,
        suggestion: Option<String>,
    },

    /// 安全警告
    #[error("Security concern at {location}: {message}")]
    Security {
        location: SourceLocation,
        message: String,
        severity: SecuritySeverity,
    },

    /// 弃用警告
    #[error("Deprecated usage at {location}: {message}")]
    Deprecated {
        location: SourceLocation,
        message: String,
        alternative: String,
    },
}

// ============================================================================
// 3. 平台错误 (Platform Errors)
// ============================================================================

#[derive(Error, Debug, Clone)]
pub enum PlatformError {
    /// 不支持的特性（已有的 UnsupportedFeatureError）
    #[error("{0}")]
    UnsupportedFeature(#[from] crate::platform_capabilities::UnsupportedFeatureError),

    /// 不兼容的运行时
    #[error("Incompatible runtime for {platform}")]
    IncompatibleRuntime {
        platform: String,
        required_runtime: String,
        found_runtime: String,
        suggestion: String,
    },

    /// 平台限制超出
    #[error("Platform limit exceeded for {platform}")]
    LimitExceeded {
        platform: String,
        limit_type: PlatformLimitType,
        limit_value: usize,
        actual_value: usize,
    },

    /// 平台不可用
    #[error("Platform {platform} is not available")]
    PlatformUnavailable {
        platform: String,
        reason: String,
    },
}

// ============================================================================
// 4. 编译错误 (Compilation Errors)
// ============================================================================

#[derive(Error, Debug, Clone)]
pub enum CompilationError {
    /// 打包器错误
    #[error("Bundler error: {message}")]
    Bundler {
        message: String,
        vendor: String,
        cause: Option<String>,
    },

    /// 适配器错误
    #[error("Adapter error for {vendor}: {message}")]
    Adapter {
        vendor: String,
        message: String,
        suggestion: Option<String>,
    },

    /// 代码转换错误
    #[error("Transform error at {location}: {message}")]
    Transform {
        location: SourceLocation,
        message: String,
        transform_stage: TransformStage,
    },

    /// 路由生成错误
    #[error("Router generation error: {message}")]
    RouterGeneration {
        message: String,
        routes: Vec<String>,
    },

    /// 依赖解析错误
    #[error("Dependency resolution error: {message}")]
    DependencyResolution {
        message: String,
        dependency: String,
        import_path: String,
    },
}

// ============================================================================
// 5. 配置错误 (Configuration Errors)
// ============================================================================

#[derive(Error, Debug, Clone)]
pub enum ConfigurationError {
    /// 无效的配置文件
    #[error("Invalid configuration in {file}: {message}")]
    InvalidConfig {
        file: String,
        message: String,
        field: Option<String>,
    },

    /// 缺少必需配置
    #[error("Missing required configuration: {field}")]
    MissingConfig {
        field: String,
        suggestion: String,
    },

    /// 配置冲突
    #[error("Configuration conflict: {message}")]
    Conflict {
        message: String,
        conflicting_fields: Vec<String>,
    },

    /// 无效的平台配置
    #[error("Invalid platform configuration for {platform}: {message}")]
    InvalidPlatformConfig {
        platform: String,
        message: String,
    },
}

// ============================================================================
// 辅助类型
// ============================================================================

/// 源码位置信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceLocation {
    pub file: String,
    pub line: usize,
    pub column: usize,
}

impl fmt::Display for SourceLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

/// 导出要求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExportRequirement {
    DefaultExport,
    NamedExport(String),
    AnyExport,
}

impl fmt::Display for ExportRequirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportRequirement::DefaultExport => write!(f, "default export"),
            ExportRequirement::NamedExport(name) => write!(f, "named export '{}'", name),
            ExportRequirement::AnyExport => write!(f, "any export"),
        }
    }
}

/// 安全严重程度
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecuritySeverity {
    Low,
    Medium,
    High,
    Critical,
}

/// 平台限制类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlatformLimitType {
    ScriptSize,
    ExecutionTime,
    MemoryUsage,
    RequestSize,
    ResponseSize,
    Concurrency,
}

impl fmt::Display for PlatformLimitType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlatformLimitType::ScriptSize => write!(f, "script size"),
            PlatformLimitType::ExecutionTime => write!(f, "execution time"),
            PlatformLimitType::MemoryUsage => write!(f, "memory usage"),
            PlatformLimitType::RequestSize => write!(f, "request size"),
            PlatformLimitType::ResponseSize => write!(f, "response size"),
            PlatformLimitType::Concurrency => write!(f, "concurrency"),
        }
    }
}

/// 转换阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransformStage {
    Parse,
    Analyze,
    Transform,
    Generate,
    Optimize,
}

impl fmt::Display for TransformStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransformStage::Parse => write!(f, "parsing"),
            TransformStage::Analyze => write!(f, "analysis"),
            TransformStage::Transform => write!(f, "transformation"),
            TransformStage::Generate => write!(f, "code generation"),
            TransformStage::Optimize => write!(f, "optimization"),
        }
    }
}

// ============================================================================
// 错误代码映射（向后兼容）
// ============================================================================

impl SpecComplianceError {
    /// 获取错误代码（用于向后兼容旧的字符串代码系统）
    pub fn error_code(&self) -> &'static str {
        match self {
            SpecComplianceError::MissingExport { .. } => "SPEC_MISSING_EXPORT",
            SpecComplianceError::PlatformDetection { .. } => "SPEC_PLATFORM_DETECTION",
            SpecComplianceError::RawAccess { .. } => "SPEC_RAW_ACCESS",
            SpecComplianceError::PlatformApi { .. } => "SPEC_PLATFORM_API",
            SpecComplianceError::ConditionalImport { .. } => "SPEC_CONDITIONAL_IMPORT",
            SpecComplianceError::InvalidHandlerSignature { .. } => "SPEC_INVALID_SIGNATURE",
        }
    }
}

impl ParseError {
    pub fn error_code(&self) -> &'static str {
        match self {
            ParseError::Syntax { .. } => "PARSE_SYNTAX",
            ParseError::InvalidStructure { .. } => "PARSE_INVALID_STRUCTURE",
            ParseError::UnsupportedSyntax { .. } => "PARSE_UNSUPPORTED_SYNTAX",
        }
    }
}

impl PlatformError {
    pub fn error_code(&self) -> &'static str {
        match self {
            PlatformError::UnsupportedFeature(_) => "PLATFORM_UNSUPPORTED_FEATURE",
            PlatformError::IncompatibleRuntime { .. } => "PLATFORM_INCOMPATIBLE_RUNTIME",
            PlatformError::LimitExceeded { .. } => "PLATFORM_LIMIT_EXCEEDED",
            PlatformError::PlatformUnavailable { .. } => "PLATFORM_UNAVAILABLE",
        }
    }
}

impl CompilationError {
    pub fn error_code(&self) -> &'static str {
        match self {
            CompilationError::Bundler { .. } => "COMPILE_BUNDLER",
            CompilationError::Adapter { .. } => "COMPILE_ADAPTER",
            CompilationError::Transform { .. } => "COMPILE_TRANSFORM",
            CompilationError::RouterGeneration { .. } => "COMPILE_ROUTER",
            CompilationError::DependencyResolution { .. } => "COMPILE_DEPENDENCY",
        }
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_hierarchy() {
        let location = SourceLocation {
            file: "test.ts".to_string(),
            line: 10,
            column: 5,
        };

        let spec_err = SpecComplianceError::PlatformDetection {
            location: location.clone(),
            detected_code: "typeof Deno !== 'undefined'".to_string(),
            suggestion: "Use context.services instead".to_string(),
        };

        assert_eq!(spec_err.error_code(), "SPEC_PLATFORM_DETECTION");

        let validation_err = ValidationError::SpecCompliance(spec_err);
        let deforge_err = DeforgeError::Validation(validation_err);

        assert!(deforge_err.to_string().contains("Platform detection"));
    }

    #[test]
    fn test_platform_error() {
        let platform_err = PlatformError::LimitExceeded {
            platform: "cloudflare".to_string(),
            limit_type: PlatformLimitType::ScriptSize,
            limit_value: 1024 * 1024,
            actual_value: 2 * 1024 * 1024,
        };

        assert_eq!(platform_err.error_code(), "PLATFORM_LIMIT_EXCEEDED");
        assert!(platform_err.to_string().contains("script size"));
    }

    #[test]
    fn test_compilation_error() {
        let compile_err = CompilationError::Bundler {
            message: "Failed to resolve module".to_string(),
            vendor: "cloudflare".to_string(),
            cause: Some("Module not found".to_string()),
        };

        assert_eq!(compile_err.error_code(), "COMPILE_BUNDLER");
    }
}
