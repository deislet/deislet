export default async function handler(context: any): Promise<Response> {
  return new Response(`root ${new URL(context.request.url).pathname}`, {
    headers: { 'content-type': 'text/plain; charset=utf-8' }
  });
}
