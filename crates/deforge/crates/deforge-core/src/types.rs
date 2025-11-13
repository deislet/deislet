use serde::{Deserialize, Serialize};

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
    pub output_path: String,
    pub files: Vec<String>,
    pub size: usize,
    pub warnings: Vec<String>,
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
