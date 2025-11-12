/**
 * Validate command - Validate project against specification
 */

import { Command } from 'commander';
import chalk from 'chalk';
import ora from 'ora';
import { Compiler } from '@denictl/compiler';

export const validateCommand = new Command('validate')
  .description('Validate project against Edge Function specification')
  .option('--strict', 'Enable strict mode (more warnings)')
  .action(async (options) => {
    const spinner = ora('Validating project...').start();

    try {
      const projectDir = process.cwd();
      const compiler = new Compiler();

      const result = await compiler.validate(projectDir, {
        strict: options.strict
      });

      if (result.valid) {
        spinner.succeed(chalk.green('✓ Validation passed'));

        if (result.warnings.length > 0) {
          console.log('');
          console.log(chalk.yellow(`⚠ ${result.warnings.length} warning(s):`));
          result.warnings.forEach((warning) => {
            console.log('');
            console.log(chalk.yellow(`  ${warning.file}:${warning.line}:${warning.column}`));
            console.log(chalk.dim(`  ${warning.message}`));
            if (warning.suggestion) {
              console.log(chalk.cyan(`  Suggestion: ${warning.suggestion}`));
            }
          });
        }

        console.log('');
        console.log(chalk.green('✓ Project structure valid'));
        console.log(chalk.green('✓ .config.json valid'));
        console.log(chalk.green(`✓ All handlers valid`));
      } else {
        spinner.fail(chalk.red('✗ Validation failed'));

        console.log('');
        console.log(chalk.red(`✗ ${result.errors.length} error(s), ${result.warnings.length} warning(s)`));

        if (result.errors.length > 0) {
          console.log('');
          console.log(chalk.bold.red('Errors:'));
          result.errors.forEach((error) => {
            console.log('');
            console.log(chalk.red(`  ${error.file}:${error.line}:${error.column}`));
            console.log(chalk.red(`  Error: ${error.message}`));
            if (error.suggestion) {
              console.log(chalk.cyan(`  Suggestion: ${error.suggestion}`));
            }
          });
        }

        if (result.warnings.length > 0) {
          console.log('');
          console.log(chalk.bold.yellow('Warnings:'));
          result.warnings.forEach((warning) => {
            console.log('');
            console.log(chalk.yellow(`  ${warning.file}:${warning.line}:${warning.column}`));
            console.log(chalk.yellow(`  Warning: ${warning.message}`));
          });
        }

        console.log('');
        console.log(chalk.red('Fix the errors above and run validate again.'));
        process.exit(1);
      }
    } catch (error) {
      spinner.fail(chalk.red('Validation error'));
      console.error(error);
      process.exit(1);
    }
  });
