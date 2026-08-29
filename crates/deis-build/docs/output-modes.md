# Build Output Modes

## Overview

Starting from version 0.1.0, deis-build supports three output modes for different use cases:

1. **Files** - Write files only (default, CLI mode)
2. **Memory** - Return in-memory content only (cloud builds)
3. **Both** - Write files and return memory content (both)

## Use Cases

### 1. OutputMode::Files (Default)

**Use for:**
- CLI command-line tools
- Local development and debugging
- When you need to inspect generated files

**Characteristics:**
- ✅ Writes all generated code to disk
- ❌ Does not return in-memory content
- `VendorBuild.artifacts` is `None`
- Minimal memory usage

**Example:**
```rust
use deis_build::{Compiler, CompilerOptions, OutputMode};

let options = CompilerOptions {
    project_dir: "./my-app".to_string(),
    output_mode: OutputMode::Files,  // default
    ..Default::default()
};

let compiler = Compiler::with_options(options);
let result = compiler.build().await?;

// Files written to dist/ directory
println!("Output: {}", result.builds["cloudflare"].output_path);
```

### 2. OutputMode::Memory

**Use for:**
- Cloud build services
- Online compilers
- CI/CD integration
- Serverless compilation
- Environments where file writes are restricted

**Characteristics:**
- ❌ Does not write files to disk
- ✅ Returns all generated code content
- `VendorBuild.artifacts` contains full content
- Can directly upload to cloud storage, CDN, etc.

### 3. OutputMode::Both

**Use for:**
- Local caching and remote deployment
- Debugging with files while auto-deploying
- Build analysis and optimization

See [Chinese version](./output-modes.zh.md) for complete documentation.
