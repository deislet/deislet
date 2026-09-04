// The Web Crypto globals, which Edge Canon makes a basic requirement
// (SPECIFICATION_BASIC.md section 8.1) and which an application reaches for
// the moment it has to sign a webhook, hash a body or hand out an id.
//
// Everything here is computed in the isolate and returned as data, so the
// verification can judge the values rather than trust a "yes". The digest is
// taken over a fixed input whose SHA-256 is a known constant, which is what
// makes it a check and not a smoke test: a `subtle.digest` that returned
// plausible-looking bytes of its own invention would pass any "did it throw?"
// test and fail this one.
export default async function handler(context: any): Promise<Response> {
  const present = {
    crypto: typeof crypto !== "undefined",
    randomUUID: typeof crypto !== "undefined" && typeof crypto.randomUUID === "function",
    getRandomValues:
      typeof crypto !== "undefined" && typeof crypto.getRandomValues === "function",
    subtle: typeof crypto !== "undefined" && typeof crypto.subtle === "object",
  };

  const answer = (body: Record<string, unknown>): Response =>
    new Response(JSON.stringify(body), {
      headers: { "content-type": "application/json; charset=utf-8" },
    });

  if (!present.crypto) {
    console.error("crypto is not defined in this isolate");
    return answer({ present, error: "crypto is not defined in this isolate" });
  }

  const out: Record<string, unknown> = { present };

  try {
    // Two of them: a generator that returns the same id twice is broken in a
    // way one sample cannot show.
    const first = crypto.randomUUID();
    const second = crypto.randomUUID();
    out.uuid = first;
    out.uuidsDiffer = first !== second;
  } catch (e) {
    out.uuidError = String(e);
  }

  try {
    // Two calls again, for the same reason, and the second is only read for
    // whether it differs - sixteen bytes that come back identical twice are
    // not random.
    const a = new Uint8Array(16);
    const b = new Uint8Array(16);
    crypto.getRandomValues(a);
    crypto.getRandomValues(b);
    out.randomBytes = a.length;
    out.randomBytesDiffer = a.some((byte: number, i: number) => byte !== b[i]);
  } catch (e) {
    out.randomError = String(e);
  }

  try {
    const digest = await crypto.subtle.digest(
      "SHA-256",
      new TextEncoder().encode("deislet"),
    );
    out.digestHex = Array.from(new Uint8Array(digest))
      .map((byte: number) => byte.toString(16).padStart(2, "0"))
      .join("");
  } catch (e) {
    out.digestError = String(e);
  }

  console.log("web crypto exercised");
  return answer(out);
}
