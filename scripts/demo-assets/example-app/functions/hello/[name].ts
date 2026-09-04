// A route with a placeholder: `functions/hello/[name].ts` becomes the pattern
// `/hello/:name`, and the captured value arrives as `context.params.name`.
export default async function handler(context: any): Promise<Response> {
  const label = context.env.BUILD_LABEL || "unknown";
  const body = {
    greeting: `Hello, ${context.params.name}!`,
    // Which version answered. The rollback step watches this field.
    label,
    // Whether the control plane delivered the secret this app declares. The
    // value itself is deliberately never put in a response or a log.
    secretPresent: typeof context.env.DEMO_SECRET === "string" && context.env.DEMO_SECRET.length > 0,
  };

  console.log(`greeted ${context.params.name} from ${label}`);

  return new Response(JSON.stringify(body), {
    headers: { "content-type": "application/json; charset=utf-8" },
  });
}
