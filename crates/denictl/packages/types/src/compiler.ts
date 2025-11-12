/**
 * Compiler interfaces
 */

export interface ValidationError {
  file: string;
  line: number;
  column: number;
  message: string;
  code: string;
  severity: 'error' | 'warning';
  suggestion?: string;
}

export interface ValidationResult {
  valid: boolean;
  errors: ValidationError[];
  warnings: ValidationError[];
}

export interface CompilationOptions {
  projectDir: string;
  vendors: string[];
  minify?: boolean;
  sourcemap?: boolean;
  watch?: boolean;
}

export interface CompilationResult {
  success: boolean;
  vendors: Record<string, VendorBuildResult>;
  duration: number;
  errors?: ValidationError[];
}

export interface VendorBuildResult {
  vendor: string;
  outputPath: string;
  files: string[];
  size: number;
  errors?: string[];
  warnings?: string[];
}

export interface HandlerInfo {
  path: string;
  route: string;
  exports: {
    hasDefault: boolean;
    namedExports: string[];
  };
  services: string[];
  dependencies: string[];
}

export interface ProjectAnalysis {
  handlers: HandlerInfo[];
  totalHandlers: number;
  services: string[];
  dependencies: string[];
}
