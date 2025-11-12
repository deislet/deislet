/**
 * Parser - Parse TypeScript/JavaScript code into AST
 */

import ts from 'typescript';

export interface ParsedAST {
  sourceFile: ts.SourceFile;
  exports: {
    hasDefault: boolean;
    namedExports: string[];
  };
  imports: string[];
  usedServices: string[];
}

export class Parser {
  /**
   * Parse source code into AST
   */
  parse(code: string, filename: string): ParsedAST {
    const sourceFile = ts.createSourceFile(
      filename,
      code,
      ts.ScriptTarget.Latest,
      true,
      ts.ScriptKind.TS
    );

    const exports = this.extractExports(sourceFile);
    const imports = this.extractImports(sourceFile);
    const usedServices = this.extractUsedServices(sourceFile);

    return {
      sourceFile,
      exports,
      imports,
      usedServices
    };
  }

  /**
   * Extract export information from AST
   */
  private extractExports(sourceFile: ts.SourceFile): {
    hasDefault: boolean;
    namedExports: string[];
  } {
    let hasDefault = false;
    const namedExports: string[] = [];

    const visit = (node: ts.Node) => {
      // Check for default export
      if (ts.isExportAssignment(node) && !node.isExportEquals) {
        hasDefault = true;
      }

      // Check for named exports
      if (ts.isFunctionDeclaration(node) && node.modifiers) {
        const hasExportModifier = node.modifiers.some(
          m => m.kind === ts.SyntaxKind.ExportKeyword
        );
        const hasDefaultModifier = node.modifiers.some(
          m => m.kind === ts.SyntaxKind.DefaultKeyword
        );

        if (hasExportModifier) {
          if (hasDefaultModifier) {
            hasDefault = true;
          } else if (node.name) {
            namedExports.push(node.name.text);
          }
        }
      }

      ts.forEachChild(node, visit);
    };

    visit(sourceFile);

    return { hasDefault, namedExports };
  }

  /**
   * Extract import statements
   */
  private extractImports(sourceFile: ts.SourceFile): string[] {
    const imports: string[] = [];

    const visit = (node: ts.Node) => {
      if (ts.isImportDeclaration(node)) {
        const moduleSpecifier = node.moduleSpecifier;
        if (ts.isStringLiteral(moduleSpecifier)) {
          imports.push(moduleSpecifier.text);
        }
      }

      ts.forEachChild(node, visit);
    };

    visit(sourceFile);

    return imports;
  }

  /**
   * Extract used services (KV, Database, etc.)
   */
  private extractUsedServices(sourceFile: ts.SourceFile): string[] {
    const services = new Set<string>();

    const visit = (node: ts.Node) => {
      // Look for context.services.kv, context.services.database, etc.
      if (ts.isPropertyAccessExpression(node)) {
        const text = node.getText(sourceFile);
        if (text.includes('context.services.kv')) {
          services.add('kv');
        }
        if (text.includes('context.services.database')) {
          services.add('database');
        }
      }

      ts.forEachChild(node, visit);
    };

    visit(sourceFile);

    return Array.from(services);
  }
}
