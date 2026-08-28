use anyhow::Result;
use colored::Colorize;
use std::fs;
use std::path::Path;

// TODO: template selection is not implemented yet; only the default template is generated
pub async fn execute(name: String, _template: String) -> Result<()> {
    println!("{}", "Creating project...".green().bold());

    let project_dir = Path::new(&name);

    // Create project structure
    fs::create_dir_all(project_dir.join("functions/api"))?;

    // Create .config.json
    let config = serde_json::json!({
        "version": "1.0",
        "name": name,
        "runtime": "standard-v1",
        "language": "typescript",
        "functionRoot": "./functions",
        "environment": {
            "variables": {
                "LOG_LEVEL": "info"
            },
            "secrets": []
        },
        "services": {
            "kv": { "enabled": true },
            "database": { "enabled": false }
        },
        "vendors": {
            "cloudflare": { "enabled": true },
            "deno": { "enabled": true },
            "tencent": { "enabled": false }
        },
        "build": {
            "outDir": "./dist",
            "minify": true,
            "sourcemap": true
        }
    });

    fs::write(
        project_dir.join(".config.json"),
        serde_json::to_string_pretty(&config)?,
    )?;

    // Create package.json
    let package_json = serde_json::json!({
        "name": name,
        "version": "0.1.0",
        "type": "module",
        "scripts": {
            "dev": "deis dev",
            "build": "deis build",
            "deploy": "deis deploy",
            "validate": "deis validate"
        },
        "devDependencies": {
            "typescript": "^5.3.3",
            "@types/node": "^20.11.5"
        }
    });

    fs::write(
        project_dir.join("package.json"),
        serde_json::to_string_pretty(&package_json)?,
    )?;

    // Create functions/index.ts
    let index_handler = r#"/**
 * Default route handler
 * Route: /
 */
export default async function handler(context: Context): Promise<Response> {
  return new Response('Hello from Edge Function!', {
    headers: { 'Content-Type': 'text/plain' }
  });
}

interface Context {
  request: Request;
  env: Record<string, string>;
  params: Record<string, string>;
  path: string;
  method: string;
  query: Record<string, string | string[]>;
  services: {
    kv?: any;
    database?: any;
  };
}
"#;

    fs::write(project_dir.join("functions/index.ts"), index_handler)?;

    // Create functions/api/hello.ts
    let hello_handler = r#"/**
 * API endpoint example
 * Route: /api/hello
 */
export default async function handler(context: Context): Promise<Response> {
  const { query } = context;
  const name = query.name || 'World';

  const response = {
    message: `Hello, ${name}!`,
    timestamp: new Date().toISOString(),
    path: context.path
  };

  return new Response(JSON.stringify(response, null, 2), {
    headers: { 'Content-Type': 'application/json' }
  });
}

interface Context {
  request: Request;
  env: Record<string, string>;
  params: Record<string, string>;
  path: string;
  method: string;
  query: Record<string, string | string[]>;
  services: {
    kv?: any;
    database?: any;
  };
}
"#;

    fs::write(project_dir.join("functions/api/hello.ts"), hello_handler)?;

    // Create .env
    fs::write(
        project_dir.join(".env"),
        "# Development environment variables\nLOG_LEVEL=debug\n",
    )?;

    // Create .env.production
    fs::write(
        project_dir.join(".env.production"),
        "# Production environment variables\nLOG_LEVEL=info\n",
    )?;

    // Create tsconfig.json
    let tsconfig = serde_json::json!({
        "compilerOptions": {
            "target": "ES2022",
            "module": "ESNext",
            "lib": ["ES2022"],
            "moduleResolution": "bundler",
            "strict": true,
            "esModuleInterop": true,
            "skipLibCheck": true,
            "forceConsistentCasingInFileNames": true
        },
        "include": ["functions/**/*"]
    });

    fs::write(
        project_dir.join("tsconfig.json"),
        serde_json::to_string_pretty(&tsconfig)?,
    )?;

    // Create README.md
    let readme = format!(
        r#"# {}

Edge Function project created with Deis.

## Quick Start

```bash
# Install dependencies
npm install

# Start development server
npm run dev

# Validate project
npm run validate

# Build for all platforms
npm run build

# Deploy to platforms
npm run deploy
```

## Project Structure

```
{}/
├── functions/          # Edge function handlers
│   ├── index.ts       # Default route (/)
│   └── api/
│       └── hello.ts   # API endpoint (/api/hello)
├── .config.json       # Project configuration
├── .env               # Development environment variables
└── package.json
```
"#,
        name, name
    );

    fs::write(project_dir.join("README.md"), readme)?;

    // Create .gitignore
    fs::write(
        project_dir.join(".gitignore"),
        "node_modules/\ndist/\n.cache/\n.env.local\n.edge-function/\n",
    )?;

    println!("\n{}", "✓ Project created successfully!".green().bold());
    println!("\n{}", "Next steps:".bold());
    println!("  {} {}", "cd".cyan(), name.cyan());
    println!("  {}", "npm install".cyan());
    println!("  {}", "npm run dev".cyan());
    println!();

    Ok(())
}
