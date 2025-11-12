/**
 * Build command - Build project for target platforms
 */

import { Command } from 'commander';
import chalk from 'chalk';
import ora from 'ora';
import { Compiler } from '@denictl/compiler';

export const buildCommand = new Command('build')
  .description('Build project for target platforms')
  .option('--vendor <vendors...>', 'Target vendors (cloudflare, deno, tencent, or all)', ['all'])
  .option('--watch', 'Watch for file changes and rebuild')
  .option('--no-minify', 'Disable minification')
  .action(async (options) => {
    const spinner = ora('Building project...').start();

    try {
      const projectDir = process.cwd();
      const compiler = new Compiler();

      // First validate
      spinner.text = 'Validating project...';
      const validation = await compiler.validate(projectDir);

      if (!validation.valid) {
        spinner.fail(chalk.red('Validation failed'));
        console.log(chalk.red(`\n✗ Fix validation errors before building`));
        process.exit(1);
      }

      spinner.text = 'Building...';

      const result = await compiler.compile({
        projectDir,
        vendors: options.vendor,
        minify: options.minify,
        sourcemap: true,
        watch: options.watch
      });

      if (result.success) {
        spinner.succeed(chalk.green('Build completed'));

        console.log('');
        const vendorNames = Object.keys(result.vendors);
        vendorNames.forEach((vendor, index) => {
          const build = result.vendors[vendor];
          console.log(chalk.bold(`[${index + 1}/${vendorNames.length}] ${vendor.charAt(0).toUpperCase() + vendor.slice(1)}`));
          console.log(chalk.green(`  ✓ Compiled ${build.files.length} file(s)`));
          console.log(chalk.green(`  ✓ Output: ${build.outputPath} (${(build.size / 1024).toFixed(2)} KB)`));

          if (build.warnings && build.warnings.length > 0) {
            console.log(chalk.yellow(`  ⚠ ${build.warnings.length} warning(s)`));
          }

          console.log('');
        });

        const totalSize = Object.values(result.vendors).reduce((sum, v) => sum + v.size, 0);
        console.log(chalk.green(`✓ Build completed in ${(result.duration / 1000).toFixed(2)}s`));
        console.log(chalk.dim(`  Total output: ${(totalSize / 1024).toFixed(2)} KB`));
      } else {
        spinner.fail(chalk.red('Build failed'));

        if (result.errors && result.errors.length > 0) {
          console.log('');
          result.errors.forEach((error) => {
            console.log(chalk.red(`  ${error.file}:${error.line} - ${error.message}`));
          });
        }

        process.exit(1);
      }
    } catch (error) {
      spinner.fail(chalk.red('Build error'));
      console.error(error);
      process.exit(1);
    }
  });
