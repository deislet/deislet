/**
 * Validator - Check code against Edge Function specification
 * Detects and prohibits platform-specific code
 */

import ts from 'typescript';
import type { ProjectConfig, ValidationError } from '@denictl/types';
import type { ParsedAST } from './parser.js';

export class Validator {
  /**
   * Validate handler code against specification
   */
  validate(
    ast: ParsedAST,
    filename: string,
    config: ProjectConfig
  ): { errors: ValidationError[]; warnings: ValidationError[] } {
    const errors: ValidationError[] = [];
    const warnings: ValidationError[] = [];

    // Check for required exports
    if (!ast.exports.hasDefault && ast.exports.namedExports.length === 0) {
      errors.push({
        file: filename,
        line: 1,
        column: 1,
        message: 'Handler must have a default export or named exports (onRequest, onRequestGet, etc.)',
        code: 'NO_EXPORTS',
        severity: 'error',
        suggestion: 'Add: export default async function handler(context: Context): Promise<Response>'
      });
    }

    // Check for platform-specific code
    this.checkPlatformSpecificCode(ast.sourceFile, filename, errors);

    // Check for prohibited patterns
    this.checkProhibitedPatterns(ast.sourceFile, filename, errors, config);

    return { errors, warnings };
  }

  /**
   * Check for platform type detection (typeof Deno, typeof Cloudflare, etc.)
   */
  private checkPlatformSpecificCode(
    sourceFile: ts.SourceFile,
    filename: string,
    errors: ValidationError[]
  ): void {
    const visit = (node: ts.Node) => {
      // Check for typeof checks
      if (ts.isBinaryExpression(node)) {
        const text = node.getText(sourceFile);

        // Detect platform type checks
        const platformChecks = [
          'typeof Deno',
          'typeof Cloudflare',
          'typeof EdgeRuntime',
          'globalThis.Deno',
          'globalThis.Cloudflare'
        ];

        for (const check of platformChecks) {
          if (text.includes(check)) {
            const { line, character } = sourceFile.getLineAndCharacterOfPosition(
              node.getStart(sourceFile)
            );

            errors.push({
              file: filename,
              line: line + 1,
              column: character + 1,
              message: 'Platform-specific code detected: You cannot check for platform type',
              code: 'PLATFORM_DETECTION',
              severity: 'error',
              suggestion: 'Use unified interfaces from context.services instead'
            });
          }
        }
      }

      // Check for conditional imports
      if (ts.isCallExpression(node)) {
        const text = node.getText(sourceFile);
        if (text.startsWith('import(') && text.includes('?')) {
          const { line, character } = sourceFile.getLineAndCharacterOfPosition(
            node.getStart(sourceFile)
          );

          errors.push({
            file: filename,
            line: line + 1,
            column: character + 1,
            message: 'Conditional imports are not allowed',
            code: 'CONDITIONAL_IMPORT',
            severity: 'error',
            suggestion: 'Use static imports only'
          });
        }
      }

      ts.forEachChild(node, visit);
    };

    visit(sourceFile);
  }

  /**
   * Check for prohibited patterns (context.raw access, direct env access)
   */
  private checkProhibitedPatterns(
    sourceFile: ts.SourceFile,
    filename: string,
    errors: ValidationError[],
    config: ProjectConfig
  ): void {
    const visit = (node: ts.Node) => {
      if (ts.isPropertyAccessExpression(node)) {
        const text = node.getText(sourceFile);

        // Check for context.raw access
        if (text.includes('context.raw')) {
          const { line, character } = sourceFile.getLineAndCharacterOfPosition(
            node.getStart(sourceFile)
          );

          errors.push({
            file: filename,
            line: line + 1,
            column: character + 1,
            message: 'Direct access to context.raw is prohibited',
            code: 'RAW_ACCESS',
            severity: 'error',
            suggestion: 'Use unified context interfaces instead'
          });
        }

        // Check for platform-specific API calls
        const platformAPIs = ['Deno.env', 'Deno.openKv', 'process.env'];
        for (const api of platformAPIs) {
          if (text.includes(api)) {
            const { line, character } = sourceFile.getLineAndCharacterOfPosition(
              node.getStart(sourceFile)
            );

            errors.push({
              file: filename,
              line: line + 1,
              column: character + 1,
              message: `Platform-specific API detected: ${api}`,
              code: 'PLATFORM_API',
              severity: 'error',
              suggestion: 'Use context.env or context.services instead'
            });
          }
        }
      }

      ts.forEachChild(node, visit);
    };

    visit(sourceFile);
  }
}
