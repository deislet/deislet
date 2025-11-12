/**
 * Main Compiler class - orchestrates validation, transformation, and building
 */

import { readFile } from 'fs/promises';
import { join } from 'path';
import type {
  ProjectConfig,
  ValidationResult,
  CompilationOptions,
  CompilationResult,
  VendorBuildResult
} from '@denictl/types';
import { Parser } from './parser.js';
import { Validator } from './validator.js';
import { Transformer } from './transformer.js';
import { loadProjectConfig, discoverHandlers } from './utils.js';

export class Compiler {
  private parser: Parser;
  private validator: Validator;
  private transformer: Transformer;

  constructor() {
    this.parser = new Parser();
    this.validator = new Validator();
    this.transformer = new Transformer();
  }

  /**
   * Validate project against specification
   */
  async validate(
    projectDir: string,
    options?: { strict?: boolean }
  ): Promise<ValidationResult> {
    try {
      // Load project configuration
      const config = await loadProjectConfig(projectDir);

      // Discover all handler files
      const handlers = await discoverHandlers(
        join(projectDir, config.functionRoot || 'functions')
      );

      // Validate each handler
      const errors: any[] = [];
      const warnings: any[] = [];

      for (const handlerPath of handlers) {
        const code = await readFile(handlerPath, 'utf-8');

        // Parse the handler
        const ast = this.parser.parse(code, handlerPath);

        // Validate against specification
        const result = this.validator.validate(ast, handlerPath, config);

        errors.push(...result.errors);
        warnings.push(...result.warnings);
      }

      return {
        valid: errors.length === 0,
        errors,
        warnings
      };
    } catch (error) {
      return {
        valid: false,
        errors: [
          {
            file: 'project',
            line: 0,
            column: 0,
            message: error instanceof Error ? error.message : 'Unknown error',
            code: 'VALIDATION_ERROR',
            severity: 'error' as const
          }
        ],
        warnings: []
      };
    }
  }

  /**
   * Compile project for target platforms
   */
  async compile(options: CompilationOptions): Promise<CompilationResult> {
    const startTime = Date.now();

    try {
      // First validate
      const validation = await this.validate(options.projectDir);
      if (!validation.valid) {
        return {
          success: false,
          vendors: {},
          duration: Date.now() - startTime,
          errors: validation.errors
        };
      }

      // Load configuration
      const config = await loadProjectConfig(options.projectDir);

      // Determine target vendors
      let targetVendors = options.vendors;
      if (targetVendors.includes('all')) {
        targetVendors = Object.entries(config.vendors || {})
          .filter(([_, v]) => v.enabled)
          .map(([name]) => name);
      }

      // Build for each vendor
      const vendors: Record<string, VendorBuildResult> = {};

      for (const vendor of targetVendors) {
        const buildResult = await this.buildForVendor(
          vendor,
          options.projectDir,
          config,
          options
        );
        vendors[vendor] = buildResult;
      }

      return {
        success: true,
        vendors,
        duration: Date.now() - startTime
      };
    } catch (error) {
      return {
        success: false,
        vendors: {},
        duration: Date.now() - startTime,
        errors: [
          {
            file: 'project',
            line: 0,
            column: 0,
            message: error instanceof Error ? error.message : 'Compilation failed',
            code: 'BUILD_ERROR',
            severity: 'error' as const
          }
        ]
      };
    }
  }

  /**
   * Build for a specific vendor
   */
  private async buildForVendor(
    vendor: string,
    projectDir: string,
    config: ProjectConfig,
    options: CompilationOptions
  ): Promise<VendorBuildResult> {
    const functionsDir = join(projectDir, config.functionRoot || 'functions');
    const outputDir = join(projectDir, config.build?.outDir || 'dist', vendor);

    // Discover handlers
    const handlers = await discoverHandlers(functionsDir);

    // Transform each handler
    const files: string[] = [];
    let totalSize = 0;

    for (const handlerPath of handlers) {
      const code = await readFile(handlerPath, 'utf-8');

      // Transform for vendor
      const transformed = await this.transformer.transform(
        code,
        handlerPath,
        vendor,
        config
      );

      files.push(handlerPath);
      totalSize += transformed.length;
    }

    return {
      vendor,
      outputPath: outputDir,
      files,
      size: totalSize,
      warnings: []
    };
  }
}
