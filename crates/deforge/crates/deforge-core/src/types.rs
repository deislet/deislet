use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 编译器输出模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    /// 只写入文件到磁盘（默认，用于 CLI）
    #[default]
    Files,
    /// 只返回内存中的内容，不写文件（用于库集成）
    Memory,
    /// 既写文件又返回内存内容（用于需要两者的场景）
    Both,
}

/// 构建产物内容（内存模式）
#[derive(Debug, Clone)]
pub struct BuildArtifacts {
    /// 主入口代码（如 index.js 或 router.ts）
    pub entry_code: String,
    /// 入口文件名
    pub entry_filename: String,
    /// 配置文件（文件名 -> 内容）
    pub config_files: HashMap<String, String>,
    /// 转换后的源文件（相对路径 -> 内容）
    pub source_files: HashMap<String, String>,
    /// 系统生成的文件（如 adapters/context-builder.ts）
    pub system_files: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationError {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub message: String,
    pub code: String,
    pub severity: Severity,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Default)]
pub struct ValidationResult {
    pub errors: Vec<ValidationError>,
    pub warnings: Vec<ValidationError>,
}

impl ValidationResult {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn add_error(&mut self, error: ValidationError) {
        self.errors.push(error);
    }

    pub fn add_warning(&mut self, warning: ValidationError) {
        self.warnings.push(warning);
    }
}

#[derive(Debug, Clone)]
pub struct CompilationResult {
    pub success: bool,
    pub builds: std::collections::HashMap<String, VendorBuild>,
    pub errors: Vec<ValidationError>,
}

#[derive(Debug, Clone)]
pub struct VendorBuild {
    pub vendor: String,
    /// 输出目录路径（Files/Both 模式有值）
    pub output_path: String,
    /// 文件路径列表（Files/Both 模式有值）
    pub files: Vec<String>,
    pub size: usize,
    pub warnings: Vec<String>,
    /// 构建产物内容（Memory/Both 模式有值）
    pub artifacts: Option<BuildArtifacts>,
}

#[derive(Debug, Clone)]
pub struct HandlerInfo {
    pub path: String,
    pub route: String,
    pub has_default_export: bool,
    pub named_exports: Vec<String>,
    pub used_services: Vec<String>,
    pub imports: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AdapterArtifacts {
    pub router_code: String,
    pub context_builder_code: String,
    pub kv_adapter_code: Option<String>,
    pub config_files: Vec<(String, String)>,
}
