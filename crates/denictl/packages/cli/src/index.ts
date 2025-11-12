#!/usr/bin/env node

/**
 * DeniCTL - Universal Edge Function CLI
 * Main entry point
 */

import { Command } from 'commander';
import { initCommand } from './commands/init.js';
import { devCommand } from './commands/dev.js';
import { buildCommand } from './commands/build.js';
import { validateCommand } from './commands/validate.js';
import { deployCommand } from './commands/deploy.js';
import { configCommand } from './commands/config.js';

const program = new Command();

program
  .name('denictl')
  .description('Universal Edge Function CLI - Write once, deploy anywhere')
  .version('0.1.0');

// Register commands
program.addCommand(initCommand);
program.addCommand(devCommand);
program.addCommand(buildCommand);
program.addCommand(validateCommand);
program.addCommand(deployCommand);
program.addCommand(configCommand);

// Parse arguments
program.parse(process.argv);
