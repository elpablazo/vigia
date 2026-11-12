// Vigía relay: fetches a page for your Vigía server from Cloudflare's network, for sites that
// reject your server's IP. Stateless, and only answers requests that carry RELAY_TOKEN.
// It does not solve challenges or CAPTCHAs: if the site challenges the relay too, it says so.

const MAX_BODY = 4_000_000;

const json = (data, status = 200) => new Response(JSON.stringify(data), { status, headers: { "content-type": "application/json" } });

async function sameSecret(a, b) {
  const enc = new TextEncoder();
  const [x, y] = await Promise.all([crypto.subtle.digest("SHA-256", enc.encode(a)), crypto.subtle.digest("SHA-256", enc.encode(b))]);
  return crypto.subtle.timingSafeEqual(x, y);
}

export default {
  async fetch(request, env) {
    if (request.method !== "POST") return json({ error: "POST only" }, 405);
    const auth = request.headers.get("authorization") ?? "";
    if (!env.RELAY_TOKEN || !(await sameSecret(auth, `Bearer ${env.RELAY_TOKEN}`))) return json({ error: "unauthorized" }, 401);

    let target, userAgent;
    try {
      const body = await request.json();
      target = new URL(body.url);
      userAgent = String(body.user_agent ?? "Vigia-relay");
    } catch {
      return json({ error: "expected {url, user_agent}" }, 400);
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
      return json({ status: res.status, challenge, body });
    } catch (e) {
      return json({ error: `fetch failed: ${e.message}` }, 502);
    }
  },
};
