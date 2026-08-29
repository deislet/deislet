export default async function handler(context: any): Promise<Response> {
  return new Response(JSON.stringify({ id: context.params.id }), {
    headers: { 'content-type': 'application/json; charset=utf-8' }
  });
}
