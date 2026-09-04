/// Build Error Classification System
///
/// Design principles:
/// 1. Clear hierarchy: layered by error kind (parse, validation, platform, compilation, configuration)
/// 2. Type safety: use enums instead of string codes
/// 3. Rich information: provide detailed messages, locations, and suggestions
/// 4. Extensibility: easy to add new error kinds

use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

// ============================================================================
// Top-level error type
// ============================================================================

#[derive(Error, Debug)]
pub enum BuildError {
    /// Parse error - code structure and syntax issues
    #[error("Parse error: {0}")]
    Parse(#[from] ParseError),

    /// Validation error - spec compliance checks
    #[error("Validation error: {0}")]
    Validation(#[from] ValidationError),

    /// Target environment or Deislet packager implementation issue.
    #[error("Platform error: {0}")]
    Platform(#[from] PlatformError),

    /// Compilation error - transform, bundling, and adapter issues
    #[error("Compilation error: {0}")]
    Compilation(#[from] CompilationError),

    /// Configuration error - project configuration issues
    #[error("Configuration error: {0}")]
    Configuration(#[from] ConfigurationError),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

// ============================================================================
// 1. Parse Errors
// ============================================================================

#[derive(Error, Debug, Clone)]
pub enum ParseError {
    /// TypeScript/JavaScript syntax error
    #[error("Syntax error at {location}: {message}")]
    Syntax {
        location: SourceLocation,
        message: String,
        snippet: Option<String>,
    },

    /// Invalid file structure
    #[error("Invalid file structure in {file}: {message}")]
    InvalidStructure {
        file: String,
        message: String,
    },

    /// Unsupported syntax feature
    #[error("Unsupported syntax at {location}: {feature}")]
    UnsupportedSyntax {
        location: SourceLocation,
        feature: String,
        suggestion: Option<String>,
    },
}

// ============================================================================
// 2. Validation Errors
// ============================================================================

#[derive(Error, Debug, Clone)]
pub enum ValidationError {
    /// Spec compliance error
    #[error("{0}")]
    SpecCompliance(#[from] SpecComplianceError),

    /// Code quality warning
    #[error("{0}")]
    CodeQuality(#[from] CodeQualityWarning),
}

/// Spec compliance errors - violations of the deislet/spec standard
#[derive(Error, Debug, Clone)]
pub enum SpecComplianceError {
    /// Missing required export
    #[error("Missing required export in {file}")]
    MissingExport {
        file: String,
        location: SourceLocation,
        expected: ExportRequirement,
        suggestion: String,
    },

    /// Platform detection code (typeof Deno etc. is forbidden)
    #[error("Platform detection code at {location}")]
    PlatformDetection {
        location: SourceLocation,
        detected_code: String,
        suggestion: String,
    },

    /// Access to context.raw (forbidden)
    #[error("Direct access to context.raw at {location}")]
    RawAccess {
        location: SourceLocation,
        accessed_property: String,
        suggestion: String,
    },

    /// Platform-specific API usage (Deno.env, process.env etc. are forbidden)
    #[error("Platform-specific API usage at {location}")]
    PlatformApi {
        location: SourceLocation,
        api_name: String,
        platform: String,
        suggestion: String,
    },

    /// Conditional import (platform-based dynamic imports are forbidden)
    #[error("Conditional import at {location}")]
    ConditionalImport {
        location: SourceLocation,
        condition: String,
        suggestion: String,
    },

    /// Invalid handler signature
    #[error("Invalid handler signature at {location}")]
    InvalidHandlerSignature {
        location: SourceLocation,
        found: String,
        expected: String,
    },
}

/// Code quality warnings - do not block compilation but improvements are recommended
#[derive(Error, Debug, Clone)]
pub enum CodeQualityWarning {
    /// Performance warning
    #[error("Performance concern at {location}: {message}")]
    Performance {
        location: SourceLocation,
        message: String,
        suggestion: Option<String>,
    },

    /// Security warning
    #[error("Security concern at {location}: {message}")]
    Security {
        location: SourceLocation,
        message: String,
        severity: SecuritySeverity,
    },

    /// Deprecation warning
    #[error("Deprecated usage at {location}: {message}")]
    Deprecated {
        location: SourceLocation,
        message: String,
        alternative: String,
    },
}

// ============================================================================
// 3. Platform Errors
// ============================================================================

#[derive(Error, Debug, Clone)]
pub enum PlatformError {
    /// Deislet's selected target packager has no mapping for this feature.
    #[error("{0}")]
    MissingTargetImplementation(
        #[from] crate::target_artifact_coverage::MissingTargetImplementationError,
    ),

    /// Incompatible runtime
    #[error("Incompatible runtime for {platform}")]
    IncompatibleRuntime {
        platform: String,
        required_runtime: String,
        found_runtime: String,
        suggestion: String,
    },

    /// Platform limit exceeded
    #[error("Platform limit exceeded for {platform}")]
    LimitExceeded {
        platform: String,
        limit_type: PlatformLimitType,
        limit_value: usize,
        actual_value: usize,
    },

    /// Platform unavailable
    #[error("Platform {platform} is not available")]
    PlatformUnavailable {
        platform: String,
        reason: String,
    },
}

// ============================================================================
// 4. Compilation Errors
// ============================================================================

#[derive(Error, Debug, Clone)]
pub enum CompilationError {
    /// Bundler error
    #[error("Bundler error: {message}")]
    Bundler {
        message: String,
        vendor: String,
        cause: Option<String>,
    },

    /// Adapter error
    #[error("Adapter error for {vendor}: {message}")]
    Adapter {
        vendor: String,
        message: String,
        suggestion: Option<String>,
    },

    /// Code transform error
    #[error("Transform error at {location}: {message}")]
    Transform {
        location: SourceLocation,
        message: String,
        transform_stage: TransformStage,
    },

    /// Route generation error
    #[error("Router generation error: {message}")]
    RouterGeneration {
        message: String,
        routes: Vec<String>,
    },

    /// Dependency resolution error
    #[error("Dependency resolution error: {message}")]
    DependencyResolution {
        message: String,
        dependency: String,
        import_path: String,
    },
}

// ============================================================================
// 5. Configuration Errors
// ============================================================================

#[derive(Error, Debug, Clone)]
pub enum ConfigurationError {
    /// Invalid configuration file
    #[error("Invalid configuration in {file}: {message}")]
    InvalidConfig {
        file: String,
        message: String,
        field: Option<String>,
    },

    /// Missing required configuration
    #[error("Missing required configuration: {field}")]
    MissingConfig {
        field: String,
        suggestion: String,
    },

    /// Conflicting configuration
    #[error("Configuration conflict: {message}")]
    Conflict {
        message: String,
        conflicting_fields: Vec<String>,
    },

    /// Invalid platform configuration
    #[error("Invalid platform configuration for {platform}: {message}")]
    InvalidPlatformConfig {
        platform: String,
        message: String,
    },
}

// ============================================================================
// Helper types
// ============================================================================

/// Source location information
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

/// Export requirement
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

/// Security severity
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecuritySeverity {
    Low,
    Medium,
    High,
    Critical,
}

/// Platform limit type
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

/// Transform stage
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
// Error code mapping (backward compatibility)
// ============================================================================

impl SpecComplianceError {
    /// Get the error code (for backward compatibility with the old string-code system)
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
            PlatformError::MissingTargetImplementation(_) => "TARGET_IMPLEMENTATION_MISSING",
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
// Tests
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
            suggestion: "Use the selected Edge Canon context and Web APIs".to_string(),
        };

        assert_eq!(spec_err.error_code(), "SPEC_PLATFORM_DETECTION");

        let validation_err = ValidationError::SpecCompliance(spec_err);
        let deis_build_err = BuildError::Validation(validation_err);

        assert!(deis_build_err.to_string().contains("Platform detection"));
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
