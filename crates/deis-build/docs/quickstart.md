# Quick Start Guide

This guide will help you build your first Edge Function with Build in 5 minutes.

## Prerequisites

- **Rust** 1.70+ ([Install Rust](https://rustup.rs/))
- A project compliant with [Edge Canon](https://github.com/deislet/edge-canon) standard

## Installing Build

### Option 1: Build from Source

```bash
# Clone the repository
git clone https://github.com/deislet/deis-build.git
cd deis-build

# Build and install
cargo build --release
cargo install --path .
```

### Option 2: Using cargo install (coming soon)

```bash
cargo install deis-build
```

### Verify Installation

```bash
deis-build --version
# Output: deis-build 0.1.0
```

## Creating an Example Project

### 1. Project Structure

Create the following directory structure:

```
my-edge-app/
├── functions/
│   ├── index.ts          # Root route /
│   └── api/
│       └── hello.ts      # /api/hello
└── .config.json          # Project configuration
```

### 2. Configuration File (.config.json)

```json
{
  "version": "0.1.0",
  "name": "my-edge-app",
  "runtime": "standard-v1",
  "language": "typescript",
  "functionRoot": "./functions",

  "vendors": {
    "cloudflare": { "enabled": true },
    "deno": { "enabled": true }
  },

  "build": {
    "outDir": "./dist",
    "minify": true
  }
}
```

### 3. Create Handlers

**functions/index.ts**:

```typescript
import type { Context } from '@edge-canon/types';

export default async function handler(context: Context): Promise<Response> {
  return new Response('Hello from Build!', {
    headers: { 'Content-Type': 'text/plain' }
  });
}
```

**functions/api/hello.ts**:

```typescript
import type { Context } from '@edge-canon/types';

export default async function handler(context: Context): Promise<Response> {
  const name = new URL(context.request.url).searchParams.get('name') || 'World';

  return new Response(JSON.stringify({
    message: `Hello, ${name}!`,
    timestamp: new Date().toISOString()
  }), {
    headers: { 'Content-Type': 'application/json' }
  });
}
```

The portable context has only `request`, `env`, `params`, and `waitUntil`.
Declare logical resources explicitly and access them as
`context.env.<BINDING_NAME>`; see [Logical resources](basic-services.md).

## Using Build

### 1. Validate Code

Run in your project directory:

```bash
deis-build validate
```

Example output:
```
✓ Validating functions/index.ts
✓ Validating functions/api/hello.ts

Validation Result:
  2/2 files passed
  ✓ No errors found
```

### 2. Build Project

#### Build for All Platforms

```bash
deis-build build
```

Output:
```
  ✓ Canonical: <artifact-digest> (content <content-root>, 7 files)
    dist/canonical/<artifact-digest>

[1/2] Cloudflare
  ✓ Derived from canonical <artifact-digest>, record <record-digest>
  ✓ Output: dist/providers/cloudflare/<payload-digest>

[2/2] Deno
  ✓ Derived from canonical <artifact-digest>, record <record-digest>
  ✓ Output: dist/providers/deno/<payload-digest>
```

#### Build for Specific Platform

```bash
# Build only for Cloudflare Workers
deis-build build --vendor cloudflare

# Build only for Deno Deploy
deis-build build --vendor deno
```

### 3. View Output

```bash
tree dist/
```

Output structure:
```
dist/
├── canonical/<artifact-digest>/     # One provider-neutral parent
├── providers/
│   ├── cloudflare/<payload-digest>/
│   │   ├── index.js
│   │   └── wrangler.toml
│   └── deno/<payload-digest>/
│       ├── _deis_internal/router.ts
│       ├── project/functions/[[all]].js
│       └── deno.json
└── .deislet/                       # Derivation records and current selectors
```

Everything under the exact `dist/providers/<vendor>/<payload-digest>/` path
printed by the build is what gets deployed and nothing else. The
build's own smoke test - `wrangler deploy --dry-run` for the Cloudflare target -
writes into a scratch directory outside the tree and removes it, so building
the same source twice produces the same bytes. It used to write its output into
the old mutable `dist/cloudflare/dist-check/`, which put wrangler's own build of the Worker and
a file with the wall-clock time into the deliverable, and made two builds of one
source differ.

## Deploy to Platforms

Build only handles building. Deployment requires platform-specific tools.

### Cloudflare Workers

```bash
cd dist/providers/cloudflare/<payload-digest>  # use the path printed by build
npx wrangler deploy
```

### Deno Deploy

```bash
cd dist/providers/deno/<payload-digest>        # use the path printed by build
deployctl deploy --project=my-app _deis_internal/router.ts
```

## Using as a Library

If you want to use Build in Rust code:

```toml
# Cargo.toml
[dependencies]
deis-build = "0.1"
tokio = { version = "1", features = ["full"] }
```

```rust
use deis_build::{Compiler, CompilerOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let options = CompilerOptions {
        project_dir: "./my-edge-app".to_string(),
        target_vendor: Some("cloudflare".to_string()),
        ..Default::default()
    };

    let compiler = Compiler::with_options(options);
    let result = compiler.build().await?;

    println!("✓ Build successful: {}", result.builds["cloudflare"].output_path);
    Ok(())
}
```

See [Library Usage Guide](./library-usage.md) for details.

## Next Steps

- 📖 Read [the deislet target](./deislet-target.md) for what a bundle contains,
  and [architecture.zh.md](../../../docs/architecture.zh.md) for the platform
- 📖 Check [Output Modes](./output-modes.md) to learn about Files/Memory/Both modes
- 📖 Read [Logical resources](./basic-services.md) to learn about KV and Cache usage
- 📖 Review [Edge Canon Specification](https://github.com/deislet/edge-canon)

## Common Questions

### What if validation fails?

```bash
# Use strict mode to see detailed errors
deis-build validate --strict
```

### How to enable extended services?

Configure in `.config.json`:

```json
{
  "services": {
    "database": {
      "enabled": true,
      "binding": "DB"
    }
  }
}
```

> **Note**: Extended services require a mapping in the current Deislet target
> packager or artifact generation fails. This is not a support declaration.

### How to add environment variables?

Environment variables are configured during deployment, not in Build's scope:

- Cloudflare: Use `wrangler.toml` or Dashboard
- Deno: Use `deployctl` or environment files

---

**Have issues?** Check [Issues](https://github.com/deislet/deis-build/issues) or submit a new one.
