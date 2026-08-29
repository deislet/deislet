export default async function handler(context: any): Promise<Response> {
  return new Response(JSON.stringify({ rest: context.params['*'] }), {
    headers: { 'content-type': 'application/json; charset=utf-8' }
  });
}
