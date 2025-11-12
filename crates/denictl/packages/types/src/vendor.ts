/**
 * Vendor/platform type definitions
 */

export type VendorName = 'cloudflare' | 'deno' | 'tencent';

export interface VendorAdapter {
  name: VendorName;
  displayName: string;

  /** Check if vendor tools are available */
  isAvailable(): Promise<boolean>;

  /** Generate platform-specific code */
  transform(sourceCode: string, options: TransformOptions): Promise<TransformResult>;

  /** Generate platform configuration files */
  generateConfig(config: any): Promise<ConfigFiles>;

  /** Deploy to platform */
  deploy(buildDir: string, options: DeployOptions): Promise<DeployResult>;
}

export interface TransformOptions {
  handlerPath: string;
  projectConfig: any;
  services: string[]; // List of services used (e.g., ['kv', 'database'])
}

export interface TransformResult {
  code: string;
  sourceMap?: string;
  dependencies: string[];
}

export interface ConfigFiles {
  [filename: string]: string;
}

export interface DeployOptions {
  environment: 'staging' | 'production';
  tag?: string;
  env?: Record<string, string>;
}

export interface DeployResult {
  success: boolean;
  deploymentId: string;
  url?: string;
  errors?: string[];
  logs?: string[];
}
