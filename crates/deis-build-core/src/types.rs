use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Compiler output mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    /// Write files to disk only (default, used by the CLI)
    #[default]
    Files,
    /// Return in-memory content only, without writing files (for library integration)
    Memory,
    /// Both write files and return in-memory content (for scenarios that need both)
    Both,
}

/// Build artifact contents (memory mode)
#[derive(Debug, Clone)]
pub struct BuildArtifacts {
    /// Main entry code (e.g. index.js or router.ts)
    pub entry_code: String,
    /// Entry file name
    pub entry_filename: String,
    /// Configuration files (file name -> content)
    pub config_files: HashMap<String, String>,
    /// Transformed source files (relative path -> content)
    pub source_files: HashMap<String, String>,
    /// System-generated files (e.g. adapters/context-builder.ts)
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
    /// Output directory path (set in Files/Both modes)
    pub output_path: String,
    /// List of file paths (set in Files/Both modes)
    pub files: Vec<String>,
    pub size: usize,
    pub warnings: Vec<String>,
    /// Build artifact contents (set in Memory/Both modes)
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
