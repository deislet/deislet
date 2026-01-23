import { v4 as uuidv4 } from 'uuid';
// @ts-ignore
import wasm from './test.wasm';

export default async function handler(context: any) {
  const id = uuidv4();
  console.log('WASM:', wasm);
  return new Response(`Hello World! ID: ${id} WASM Type: ${typeof wasm}`, {
    headers: { 'content-type': 'text/plain' }
  });
}