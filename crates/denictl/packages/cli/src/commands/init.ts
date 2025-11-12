/**
 * Init command - Initialize a new Edge Function project
 */

import { Command } from 'commander';
import { mkdir, writeFile } from 'fs/promises';
import { join } from 'path';
import chalk from 'chalk';
import ora from 'ora';

export const initCommand = new Command('init')
  .description('Initialize a new Edge Function project')
  .argument('[project-name]', 'Project name', 'edge-app')
  .option('--template <template>', 'Project template', 'default')
  .action(async (projectName: string, options) => {
    const spinner = ora('Creating project...').start();

    try {
      const projectDir = join(process.cwd(), projectName);

      // Create project structure
      await mkdir(projectDir, { recursive: true });
      await mkdir(join(projectDir, 'functions'), { recursive: true });
      await mkdir(join(projectDir, 'functions', 'api'), { recursive: true });

      // Create .config.json
      const config = {
        version: '1.0',
        name: projectName,
        runtime: 'standard-v1',
        language: 'typescript',
        functionRoot: './functions',
        environment: {
          variables: {
            LOG_LEVEL: 'info'
          },
          secrets: []
        },
        services: {
          kv: { enabled: true },
          database: { enabled: false }
        },
        vendors: {
          cloudflare: { enabled: true },
          deno: { enabled: true },
          tencent: { enabled: false }
        },
        build: {
          outDir: './dist',
          minify: true,
          sourcemap: true
        }
      };

      await writeFile(
        join(projectDir, '.config.json'),
        JSON.stringify(config, null, 2)
      );

      // Create package.json
      const packageJson = {
        name: projectName,
        version: '0.1.0',
        type: 'module',
        scripts: {
          dev: 'denictl dev',
          build: 'denictl build',
          deploy: 'denictl deploy',
          validate: 'denictl validate'
        },
        dependencies: {
          '@denictl/types': '^0.1.0'
        },
        devDependencies: {
          'typescript': '^5.3.3',
          '@types/node': '^20.11.5'
        }
      };

      await writeFile(
        join(projectDir, 'package.json'),
        JSON.stringify(packageJson, null, 2)
      );

      // Create functions/index.ts
      const indexHandler = `import type { Context } from '@denictl/types';

/**
 * Default route handler
 * Route: /
 */
export default async function handler(context: Context): Promise<Response> {
  return new Response('Hello from Edge Function!', {
    headers: { 'Content-Type': 'text/plain' }
  });
}
`;

      await writeFile(join(projectDir, 'functions', 'index.ts'), indexHandler);

      // Create functions/api/hello.ts
      const helloHandler = `import type { Context } from '@denictl/types';

/**
 * API endpoint example
 * Route: /api/hello
 */
export default async function handler(context: Context): Promise<Response> {
  const { query } = context;
  const name = query.name || 'World';

  const response = {
    message: \`Hello, \${name}!\`,
    timestamp: new Date().toISOString(),
    path: context.path
  };

  return new Response(JSON.stringify(response, null, 2), {
    headers: { 'Content-Type': 'application/json' }
  });
}
`;

      await writeFile(
        join(projectDir, 'functions', 'api', 'hello.ts'),
        helloHandler
      );

      // Create .env
      const envContent = `# Development environment variables
LOG_LEVEL=debug
`;

      await writeFile(join(projectDir, '.env'), envContent);

      // Create .env.production
      const envProdContent = `# Production environment variables
LOG_LEVEL=info
`;

      await writeFile(join(projectDir, '.env.production'), envProdContent);

      // Create tsconfig.json
      const tsconfig = {
        compilerOptions: {
          target: 'ES2022',
          module: 'ESNext',
          lib: ['ES2022'],
          moduleResolution: 'bundler',
          strict: true,
          esModuleInterop: true,
          skipLibCheck: true,
          forceConsistentCasingInFileNames: true
        },
        include: ['functions/**/*']
      };

      await writeFile(
        join(projectDir, 'tsconfig.json'),
        JSON.stringify(tsconfig, null, 2)
      );

      // Create README.md
      const readme = `# ${projectName}

Edge Function project created with DeniCTL.

## Quick Start

\`\`\`bash
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
\`\`\`

## Project Structure

\`\`\`
${projectName}/
├── functions/          # Edge function handlers
│   ├── index.ts       # Default route (/)
│   └── api/
│       └── hello.ts   # API endpoint (/api/hello)
├── .config.json       # Project configuration
├── .env               # Development environment variables
└── package.json
\`\`\`

## Learn More

- Documentation: https://github.com/deislet/spec
- Examples: https://github.com/deislet/examples
`;

      await writeFile(join(projectDir, 'README.md'), readme);

      // Create .gitignore
      const gitignore = `node_modules/
dist/
.cache/
.env.local
.edge-function/
`;

      await writeFile(join(projectDir, '.gitignore'), gitignore);

      spinner.succeed(chalk.green('Project created successfully!'));

      console.log('\n' + chalk.bold('Next steps:'));
      console.log(chalk.cyan(`  cd ${projectName}`));
      console.log(chalk.cyan('  npm install'));
      console.log(chalk.cyan('  npm run dev'));
      console.log('');
    } catch (error) {
      spinner.fail(chalk.red('Failed to create project'));
      console.error(error);
      process.exit(1);
    }
  });
