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
    /// Runtime module edges with enough information to validate an exact
    /// application surface. `imports` above remains the deduplicated graph
    /// used by staging; these records deliberately preserve how a module was
    /// accessed, because `import { createHash }` and `import * as crypto`
    /// expose different Node APIs even though they name the same specifier.
    pub module_references: Vec<ModuleReference>,
    /// `import(expr)` / `require(expr)` edges whose target is not a string
    /// literal and therefore cannot be put into a self-contained artifact.
    pub non_literal_module_loads: usize,
    pub non_literal_module_load_positions: Vec<ModuleSourcePosition>,
    /// Runtime references to globals that exist on one provider but are not
    /// in the unified application surface. These are recorded from the AST so
    /// comments, string contents and erased TypeScript types do not trigger a
    /// source-policy failure.
    pub provider_only_global_references: Vec<ProviderOnlyGlobalReference>,
    /// Direct construction of stream globals that the current portable
    /// surface does not define. Resolution context and source coordinates are
    /// preserved so a local class with the same spelling remains valid and a
    /// rejected build can point at the author's expression.
    pub non_portable_stream_constructors: Vec<NonPortableStreamConstructorReference>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleSourcePosition {
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderOnlyGlobalReference {
    pub name: String,
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonPortableStreamConstructorReference {
    pub name: String,
    pub line: usize,
    pub column: usize,
}

/// The syntax that caused a runtime module edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleReferenceKind {
    StaticImport,
    ReExport,
    DynamicImport,
    Require,
}

/// One statically named runtime module edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleReference {
    pub specifier: String,
    /// Export names read from the dependency. The original name is recorded
    /// for a renamed import/re-export (`x as y` reads `x`). `default` is kept
    /// explicit because the Edge Canon Node surface has no implicit default.
    pub requested_exports: Vec<String>,
    /// Namespace-producing syntax (`import *`, `export *`, `import()` or
    /// `require()`) can expose more than the named allow-list.
    pub loads_namespace: bool,
    pub kind: ModuleReferenceKind,
    /// One-based source coordinates for stable pre-deploy diagnostics.
    pub line: usize,
    pub column: usize,
}

/// One static file the project ships, as an adapter needs to know it.
///
/// Path relative to the assets root, no leading slash and `/` separators - the
/// same spelling `deis_bundle::Asset::path` uses - plus the media type this
/// platform serves it as. Not the bytes: copying the files is the compiler's
/// job, and an adapter only has to decide where its host expects them and what
/// the host has to be told.
///
/// The list reaches every adapter because until 2026-09-02 it reached only one.
/// Three of the four targets built a deployable directory with the project's
/// `public/` nowhere in it and said nothing, so a site that served its own CSS
/// on this platform 404ed on the other three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticAsset {
    pub path: String,
    pub content_type: String,
}

#[derive(Debug, Clone, Default)]
pub struct AdapterArtifacts {
    pub router_code: String,
    pub context_builder_code: String,
    pub kv_adapter_code: Option<String>,
    pub config_files: Vec<(String, String)>,
}
