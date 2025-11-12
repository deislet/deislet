/**
 * Project configuration (.config.json)
 */

export interface ProjectConfig {
  /** Config version */
  version: string;

  /** Project name */
  name: string;

  /** Runtime version */
  runtime: string;

  /** Programming language */
  language: 'typescript' | 'javascript';

  /** Root directory for function files */
  functionRoot?: string;

  /** Routing configuration */
  routing?: RoutingConfig;

  /** Environment variables and secrets */
  environment?: EnvironmentConfig;

  /** Service configurations */
  services?: ServicesConfig;

  /** Vendor/platform configurations */
  vendors?: VendorsConfig;

  /** Build options */
  build?: BuildConfig;

  /** Resource limits */
  limits?: LimitsConfig;
}

export interface RoutingConfig {
  /** Case-sensitive routing */
  caseSensitive?: boolean;

  /** Trailing slash handling */
  trailingSlash?: 'always' | 'never' | 'ignore';

  /** Custom route patterns */
  routes?: Record<string, string>;
}

export interface EnvironmentConfig {
  /** Public environment variables */
  variables?: Record<string, string>;

  /** Secret names (values stored separately) */
  secrets?: string[];
}

export interface ServicesConfig {
  kv?: KVServiceConfig;
  database?: DatabaseServiceConfig;
}

export interface KVServiceConfig {
  enabled: boolean;
  namespace?: string;
}

export interface DatabaseServiceConfig {
  enabled: boolean;
  connectionString?: string;
}

export interface VendorsConfig {
  cloudflare?: VendorConfig;
  deno?: VendorConfig;
  tencent?: VendorConfig;
  [key: string]: VendorConfig | undefined;
}

export interface VendorConfig {
  enabled: boolean;
  /** Vendor-specific options (opaque to spec) */
  options?: Record<string, any>;
}

export interface BuildConfig {
  /** Output directory */
  outDir?: string;

  /** Enable minification */
  minify?: boolean;

  /** Generate source maps */
  sourcemap?: boolean;

  /** Target ECMAScript version */
  target?: string;
}

export interface LimitsConfig {
  /** Maximum execution time in seconds */
  timeout?: number;

  /** Maximum request/response payload size in bytes */
  maxPayloadSize?: number;

  /** Maximum concurrent requests */
  maxConcurrency?: number;
}
