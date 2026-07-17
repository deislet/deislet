# Deforge Adapter Platform Specifications

This directory contains platform-specific specification documents for each deforge adapter. These `spec.mdc` files serve as the authoritative reference for implementing adapters that are fully compliant with each platform's requirements.

## Available Specifications

### 1. Cloudflare Workers (`deforge-adapter-cf/spec.mdc`)

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

### 2. Deno Deploy (`deforge-adapter-dc/spec.mdc`)

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

### 3. EdgeOne Pages (`deforge-adapter-eo/spec.mdc`)

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

Each `spec.mdc` file provides:

1. **Official API surface** - What's available in the runtime
2. **Entry point requirements** - How to structure the main handler
3. **Platform-specific features** - Storage, databases, special APIs
4. **Limitations and constraints** - What's not allowed or limited
5. **Best practices** - Recommended patterns and approaches
6. **Configuration formats** - How to configure projects

### For LLM/AI Assistance

These files are formatted in `.mdc` (Markdown Context) format, making them ideal for:

- **Cursor AI** - Place in `.cursor/rules/` for project-specific guidance
- **Claude/ChatGPT** - Reference when asking for platform-specific help
- **Code generation** - Ensure generated code follows platform specs

### Compliance Checklist

When implementing or updating an adapter, verify:

- ✅ Entry point format matches spec exactly
- ✅ Only uses APIs listed in the spec
- ✅ Respects platform limitations
- ✅ Follows configuration file format
- ✅ Implements error handling as specified
- ✅ Uses platform-specific best practices

## Comparison Matrix

| Feature | Cloudflare Workers | Deno Deploy | EdgeOne Pages |
|---------|-------------------|-------------|---------------|
| **Runtime** | workerd (V8) | Deno 2.x (V8) | V8 + Node.js |
| **Entry Format** | `export default { fetch() }` | `Deno.serve()` | `export function onRequest()` |
| **TypeScript** | Via bundler | Native | Via bundler |
| **KV Storage** | ✅ KV | ✅ Deno KV | ✅ EdgeKV |
| **SQL Database** | ✅ D1 | ✅ Postgres integration | ❌ |
| **Object Storage** | ✅ R2 | ❌ | ❌ |
| **Edge Functions** | ✅ Workers | ✅ Deploy | ✅ Edge Functions |
| **Node Runtime** | ❌ | ✅ Node compat | ✅ Node Functions |
| **SSR/ISR** | ❌ | ✅ | ✅ |
| **Cron Jobs** | ✅ Cron Triggers | ✅ Deno.cron | ❌ |
| **WebSockets** | ✅ | ✅ | ❌ |
| **Cold Start** | ~10ms | Near-instant | Fast |

## Maintenance

These specifications should be updated when:

1. Platform releases major runtime updates
2. New APIs become available
3. Limitations or constraints change
4. Best practices evolve

**Last Updated**: 2026-01-24

## References

- [Cloudflare Workers Documentation](https://developers.cloudflare.com/workers/)
- [Deno Deploy Documentation](https://docs.deno.com/deploy/)
- [EdgeOne Pages Documentation](https://pages.edgeone.ai/)
- [llms.txt Standard](https://llmstxthub.com/)
