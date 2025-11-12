/**
 * Utility functions for compiler
 */

import { readFile, readdir, stat } from 'fs/promises';
import { join, relative, extname } from 'path';
import type { ProjectConfig } from '@denictl/types';

/**
 * Load project configuration from .config.json
 */
export async function loadProjectConfig(projectDir: string): Promise<ProjectConfig> {
  const configPath = join(projectDir, '.config.json');

  try {
    const content = await readFile(configPath, 'utf-8');
    return JSON.parse(content) as ProjectConfig;
  } catch (error) {
    throw new Error(`Failed to load .config.json: ${error}`);
  }
}

/**
 * Discover all handler files in functions directory
 */
export async function discoverHandlers(functionsDir: string): Promise<string[]> {
  const handlers: string[] = [];

  async function walk(dir: string) {
    const entries = await readdir(dir);

    for (const entry of entries) {
      const fullPath = join(dir, entry);
      const stats = await stat(fullPath);

      if (stats.isDirectory()) {
        await walk(fullPath);
      } else if (stats.isFile()) {
        const ext = extname(entry);
        if (ext === '.ts' || ext === '.js' || ext === '.tsx' || ext === '.jsx') {
          handlers.push(fullPath);
        }
      }
    }
  }

  try {
    await walk(functionsDir);
  } catch (error) {
    throw new Error(`Failed to discover handlers: ${error}`);
  }

  return handlers;
}

/**
 * Convert file path to route
 * Example: functions/api/users/[id].ts -> /api/users/:id
 */
export function pathToRoute(filePath: string, functionsRoot: string): string {
  const relativePath = relative(functionsRoot, filePath);
  const withoutExt = relativePath.replace(/\.(ts|js|tsx|jsx)$/, '');

  // Handle index files
  let route = withoutExt.replace(/\/index$/, '');
  if (route === 'index') {
    route = '';
  }

  // Convert dynamic segments: [id] -> :id
  route = route.replace(/\[([^\]]+)\]/g, ':$1');

  // Convert catch-all: [[...catch]] -> *
  route = route.replace(/\[\[\.\.\.([^\]]+)\]\]/g, '*');

  return '/' + route;
}

/**
 * Determine if a file is a handler
 */
export function isHandlerFile(filename: string): boolean {
  const ext = extname(filename);
  return ['.ts', '.js', '.tsx', '.jsx'].includes(ext);
}
