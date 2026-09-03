# Build Adapter Platform Specifications

This directory indexes historical, platform-specific research notes used while
building the existing adapters. The `spec.mdc` files are not normative Edge
Canon sources, complete vendor specifications, or conformance evidence. Their
URLs are not pinned snapshots, so every claim must be rechecked against a
versioned official source before it affects an adapter.

Application behavior comes from the locked Edge Canon contract. Target output
shape comes from version-pinned official vendor documentation and CLI/API
behavior. Evidence, certification and support status comes only from the
[registry-derived target status](../../../docs/product/target-status.zh.md).

## Available Specifications

### 1. Cloudflare Workers (`deis-build-adapter-cloudflare/spec.mdc`)

**Platform**: Cloudflare Workers
**Size**: ~13 KB
**Source**: https://developers.cloudflare.com/workers/

**Key Topics:**
- Entry point format (modules syntax vs service workers)
- Handler parameters (request, env, ctx)
- Storage services (KV, D1, R2, Durable Objects)
- Environment variables and secrets
- Web APIs available
- Platform limitations and constraints
- Local development with Wrangler
- Deployment and routing
- Best practices and security

**Runtime**: V8 (workerd)
**Languages**: JavaScript, TypeScript, Python, Rust, WebAssembly

---

### 2. Deno Deploy (`deis-build-adapter-deno/spec.mdc`)

**Platform**: Deno Deploy
**Size**: ~17 KB
**Source**: https://docs.deno.com/deploy/

**Key Topics:**
- Entry point format (Deno.serve)
- Handler parameters (Request/Response)
- Deno APIs (filesystem, network, crypto)
- Deno KV database
- Database integrations (Postgres, MongoDB)
- Node.js compatibility
- Scheduled tasks (Cron)
- BroadcastChannel for cross-instance communication
- Platform limitations
- Local development and testing
- Deployment via deployctl and GitHub

**Runtime**: Deno 2.x (V8)
**Languages**: JavaScript, TypeScript (native), WebAssembly

---

### 3. EdgeOne Pages (`deis-build-adapter-tencent/spec.mdc`)

**Platform**: Tencent EdgeOne Pages
**Size**: ~46 KB
**Source**: https://pages.edgeone.ai/

**Key Topics:**
- Full-stack architecture (SSG + SSR + ISR + Edge Functions + Node Functions)
- Intelligent code placement system
- Edge Functions specifications
- Node Functions specifications
- Web APIs available (approved list)
- Framework support (Next.js, Gatsby, Vue, React)
- Platform integrations (Shopify, Contentful, Sanity, Supabase, Stripe)
- Internationalization (i18n) patterns
- Configuration (edgeone.json)
- Local development
- Best practices and security

**Runtime**: Edge Functions (V8-like), Node Functions (Node.js)
**Languages**: JavaScript, TypeScript

---

## Usage for Adapter Development

Each `spec.mdc` file may provide research input about:

1. **Observed API surface** - What the note claimed was available
2. **Entry point requirements** - How to structure the main handler
3. **Platform-specific features** - Storage, databases, special APIs
4. **Limitations and constraints** - What's not allowed or limited
5. **Best practices** - Recommended patterns and approaches
6. **Configuration formats** - How to configure projects

### 为什么是 `.mdc`

这些文件用 `.mdc`（Markdown Context）格式，是为了让一份规范同时给人读、也能直接
喂给编辑器的规则目录（例如放进 `.cursor/rules/`）。写新适配器或改旧的时候，它是
判断产物对不对的依据。

### Adapter artifact checklist

When implementing or updating an adapter, verify the generated artifact. Passing
this checklist does not establish Edge Canon conformance:

- [ ] Entry point format matches the pinned vendor source and tool version
- [ ] Generated code only uses APIs verified for that target
- [ ] Configuration validates with the pinned official CLI/API
- [ ] Artifact fixtures and deployment smoke tests record exact tool versions
- [ ] Shared behavior is judged by Edge Canon tests, not this note

## No hand-maintained comparison matrix

The former table mixed vendor marketing, product capability and adapter output
coverage, and had no versioned evidence. It has been removed. Cross-target
behavior is defined once by Edge Canon and evaluated by the shared suite;
target status is generated from the locked registry.

## Maintenance

These specifications should be updated when:

1. Platform releases major runtime updates
2. New APIs become available
3. Limitations or constraints change
4. Best practices evolve

**Snapshot date recorded by the original notes**: 2026-01-24. This is not a
freshness guarantee.

## References

- [Cloudflare Workers Documentation](https://developers.cloudflare.com/workers/)
- [Deno Deploy Documentation](https://docs.deno.com/deploy/)
- [EdgeOne Pages Documentation](https://pages.edgeone.ai/)
- [llms.txt Standard](https://llmstxthub.com/)
