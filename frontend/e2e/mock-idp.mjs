// A mock OpenID Connect provider for browser demonstrations: discovery,
// JWKS, an authorization page with a "Continue" button, and a token
// endpoint that enforces client_secret_basic, PKCE (S256), single-use codes
// and the registered redirect URI. The signing key is generated per run and
// never written to disk. Test use only.

import { createHash, generateKeyPairSync, randomUUID, sign } from "node:crypto";
import { once } from "node:events";
import { createServer } from "node:http";

const b64url = (buffer) => Buffer.from(buffer).toString("base64url");
const escapeHtml = (s) => String(s).replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

export async function startMockIdp({ port, clientId, clientSecret, subject, name = "Mock teacher" }) {
  const issuer = `http://127.0.0.1:${port}`;
  const { privateKey, publicKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
  const kid = `mock-${randomUUID().slice(0, 8)}`;
  const jwk = { ...publicKey.export({ format: "jwk" }), kid, alg: "RS256", use: "sig" };
  const codes = new Map();
  const events = [];

  const signJwt = (claims) => {
    const header = b64url(JSON.stringify({ alg: "RS256", typ: "JWT", kid }));
    const body = b64url(JSON.stringify(claims));
    const signature = sign("sha256", Buffer.from(`${header}.${body}`), privateKey);
    return `${header}.${body}.${b64url(signature)}`;
  };

  // RFC 6749 §2.3.1: form-encode both parts, then Basic.
  const formEncode = (value) => new URLSearchParams({ v: value }).toString().slice(2);
  const expectedBasic = `Basic ${Buffer.from(`${formEncode(clientId)}:${formEncode(clientSecret)}`).toString("base64")}`;

  const server = createServer(async (req, res) => {
    const url = new URL(req.url, issuer);
    const json = (status, body) => {
      res.writeHead(status, { "Content-Type": "application/json", "Cache-Control": "no-store" });
      res.end(JSON.stringify(body));
    };
    if (url.pathname === "/.well-known/openid-configuration") {
      return json(200, {
        issuer,
        authorization_endpoint: `${issuer}/authorize`,
        token_endpoint: `${issuer}/token`,
        jwks_uri: `${issuer}/jwks`,
        response_types_supported: ["code"],
        subject_types_supported: ["public"],
        id_token_signing_alg_values_supported: ["RS256"],
        code_challenge_methods_supported: ["S256"],
      });
    }
    if (url.pathname === "/jwks") return json(200, { keys: [jwk] });
    if (url.pathname === "/authorize") {
      const p = Object.fromEntries(url.searchParams);
      events.push({ event: "authorize", params: { ...p } });
      const ok = p.response_type === "code" && p.client_id === clientId && p.code_challenge_method === "S256" && p.scope?.split(" ").includes("openid");
      if (!ok) return json(400, { error: "invalid_request" });
      const approve = new URL("/approve", issuer);
      for (const [k, v] of Object.entries(p)) approve.searchParams.set(k, v);
      res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
      return res.end(`<!doctype html><html lang="en"><head><title>Mock identity provider</title>
<style>body{font-family:system-ui;max-width:32rem;margin:4rem auto;padding:0 1rem}a{display:inline-block;background:#1f5f99;color:#fff;padding:.6rem 1.2rem;border-radius:6px;text-decoration:none}</style></head>
<body><h1>Mock identity provider</h1><p>${escapeHtml(clientId)} asks to sign you in.</p>
<p>Account: <strong>${escapeHtml(name)}</strong> (subject <code>${escapeHtml(subject)}</code>)</p>
<a href="${escapeHtml(approve.pathname + approve.search)}">Continue</a></body></html>`);
    }
    if (url.pathname === "/approve") {
      const p = Object.fromEntries(url.searchParams);
      const code = `code-${randomUUID()}`;
      codes.set(code, { nonce: p.nonce, challenge: p.code_challenge, redirectUri: p.redirect_uri });
      const back = new URL(p.redirect_uri);
      back.searchParams.set("code", code);
      back.searchParams.set("state", p.state);
      res.writeHead(302, { Location: back.toString() });
      return res.end();
    }
    if (url.pathname === "/token" && req.method === "POST") {
      let raw = "";
      for await (const chunk of req) raw += chunk;
      const form = Object.fromEntries(new URLSearchParams(raw));
      const basicOk = req.headers.authorization === expectedBasic;
      const grant = codes.get(form.code);
      codes.delete(form.code);
      const pkceOk = grant && b64url(createHash("sha256").update(form.code_verifier ?? "").digest()) === grant.challenge;
      events.push({ event: "token", basicOk, grant: Boolean(grant), pkceOk: Boolean(pkceOk), redirectOk: grant?.redirectUri === form.redirect_uri });
      if (!basicOk) return json(401, { error: "invalid_client" });
      if (form.grant_type !== "authorization_code" || !grant || !pkceOk || grant.redirectUri !== form.redirect_uri) {
        return json(400, { error: "invalid_grant" });
      }
      const now = Math.floor(Date.now() / 1000);
      const idToken = signJwt({ iss: issuer, sub: subject, aud: clientId, iat: now, exp: now + 300, nonce: grant.nonce, name });
      return json(200, { access_token: "opaque", token_type: "Bearer", expires_in: 300, id_token: idToken });
    }
    json(404, { error: "not_found" });
  });
  server.listen(port, "127.0.0.1");
  await once(server, "listening");
  return {
    issuer,
    events,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}
