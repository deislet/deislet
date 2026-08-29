export default async function handler(context: any): Promise<Response> {
  return new Response(`root ${context.path}`, {
    headers: { 'content-type': 'text/plain; charset=utf-8' }
  });
}
