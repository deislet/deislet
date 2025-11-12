/**
 * Handler function signatures
 */

import type { Context } from './context.js';

/**
 * Universal handler function - handles any HTTP method
 */
export type Handler = (context: Context) => Promise<Response> | Response;

/**
 * Method-specific handler functions
 */
export type HandlerGet = (context: Context) => Promise<Response> | Response;
export type HandlerPost = (context: Context) => Promise<Response> | Response;
export type HandlerPut = (context: Context) => Promise<Response> | Response;
export type HandlerDelete = (context: Context) => Promise<Response> | Response;
export type HandlerPatch = (context: Context) => Promise<Response> | Response;
export type HandlerHead = (context: Context) => Promise<Response> | Response;
export type HandlerOptions = (context: Context) => Promise<Response> | Response;

/**
 * Named exports that edge functions can provide
 */
export interface NamedHandlerExports {
  onRequest?: Handler;
  onRequestGet?: HandlerGet;
  onRequestPost?: HandlerPost;
  onRequestPut?: HandlerPut;
  onRequestDelete?: HandlerDelete;
  onRequestPatch?: HandlerPatch;
  onRequestHead?: HandlerHead;
  onRequestOptions?: HandlerOptions;
}

/**
 * Middleware function - can transform context or response
 */
export type Middleware = (
  context: Context,
  next: () => Promise<Response>
) => Promise<Response>;
