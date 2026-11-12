// Vigía relay: fetches a page for your Vigía server from Cloudflare's network, for sites that
// reject your server's IP. Stateless. It only obeys requests signed by your server: the server keeps
// the private key and this file holds only the public one, so nothing secret is stored here.
// It does not solve challenges or CAPTCHAs: if the site challenges the relay too, it says so.
//
// Get this file with your key already filled in from your Vigía: Ajustes → Relevo → Copiar código.

const PUBLIC_KEY = "PASTE_PUBLIC_KEY_HERE"; // base64url, uncompressed P-256 point
const MAX_BODY = 4_000_000;
const MAX_SKEW = 120; // seconds a signed request stays valid

const json = (data, status = 200) => new Response(JSON.stringify(data), { status, headers: { "content-type": "application/json" } });
const unb64 = (s) => Uint8Array.from(atob(s.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0));

let key;
async function verify(message, signature) {
  key ??= await crypto.subtle.importKey("raw", unb64(PUBLIC_KEY), { name: "ECDSA", namedCurve: "P-256" }, false, ["verify"]);
  return crypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, key, unb64(signature), new TextEncoder().encode(message));
}

export default {
  async fetch(request) {
    if (request.method !== "POST") return json({ error: "POST only" }, 405);
    let url, userAgent;
    try {
      ({ url, user_agent: userAgent } = await request.json());
      url = String(url);
      userAgent = String(userAgent ?? "Vigia-relay");
    } catch {
      return json({ error: "expected {url, user_agent}" }, 400);
    }

    const time = request.headers.get("x-vigia-time") ?? "";
    const signature = request.headers.get("x-vigia-signature") ?? "";
    const fresh = Math.abs(Date.now() / 1000 - Number(time)) < MAX_SKEW;
    let valid = false;
    try {
      valid = fresh && (await verify(`${time}\n${url}\n${userAgent}`, signature));
    } catch {}
    if (!valid) return json({ error: "unauthorized" }, 401);

    let target;
    try {
      target = new URL(url);
    } catch {
      return json({ error: "bad url" }, 400);
    }
    if (target.protocol !== "http:" && target.protocol !== "https:") return json({ error: "http(s) only" }, 400);

    try {
      const res = await fetch(target, {
        headers: {
          "user-agent": userAgent,
          accept: "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.8",
          "accept-language": "es-MX,es;q=0.9,en;q=0.8",
        },
        redirect: "follow",
        signal: AbortSignal.timeout(25_000),
      });
      const challenge = (res.headers.get("cf-mitigated") ?? "").toLowerCase() === "challenge";
      const body = (await res.text()).slice(0, MAX_BODY);
      const content_type = (res.headers.get("content-type") ?? "").toLowerCase();
      return json({ status: res.status, challenge, body, content_type });
    } catch (e) {
      return json({ error: `fetch failed: ${e.message}` }, 502);
    }
  },
};
