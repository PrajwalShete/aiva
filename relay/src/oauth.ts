// Stateless OAuth 2.1 authorization server for MCP clients (Claude, ChatGPT, Cursor...).
// Nothing is stored: registered clients, pending authorizations, codes and tokens are all
// HMAC-signed payloads. "Signing in" means pasting the computer's key (shown by `aiva setup`),
// which proves ownership of the connector URL because the URL is derived from that key.
// Follows the MCP authorization spec and Claude's connector requirements: RFC 9728 metadata
// per computer (resource = the exact connector URL), RFC 8414, RFC 7591 registration, PKCE S256.

import { connectPage } from "./pages";
import { bytesToHex, concat, derive, fromB64url, hexToBytes, idFromKey, json, looksLikeKey, nowSec, packSigned, readBody, readU32, redirect, sha256Bytes, sign, u32, unpackSigned, verify, withParams } from "./util";

export const SCOPE = "mcp";
const ACCESS_TTL = 3600;
const REFRESH_TTL = 90 * 86400;
const CODE_TTL = 120;
const REQUEST_TTL = 1800;

type Client = { t: "client"; r: string[]; n: string | null; m: string };
type PendingAuth = { t: "req"; c: string; r: string; s: string | null; cc: string; id: string | null; exp: number };
// Compact token layouts (after the type byte): exp u32 · computer id (10 bytes) · binding (8 bytes)
// [· code challenge prefix (12 bytes), codes only]. The binding is SHA-256 of the client id (plus
// redirect URI for codes), so a code or token only works for the app it was issued to.
const CLIENT = 4;
const CODE = 1;
const ACCESS = 2;
const REFRESH = 3;

async function binding(...parts: string[]): Promise<Uint8Array> {
  return (await sha256Bytes(parts.join("\n"))).slice(0, 8);
}

type Grant = { exp: number; id: string; bind: Uint8Array; rest: Uint8Array };

async function packGrant(env: Env, type: number, ttl: number, id: string, bind: Uint8Array, rest: Uint8Array = new Uint8Array()): Promise<string> {
  return packSigned(env, type, concat(u32(nowSec() + ttl), hexToBytes(id), bind, rest));
}

async function unpackGrant(env: Env, token: string | undefined, type: number): Promise<Grant | null> {
  const f = await unpackSigned(env, token, type);
  if (!f || f.length < 22) return null;
  const exp = readU32(f, 0);
  if (exp < nowSec()) return null;
  return { exp, id: bytesToHex(f.slice(4, 14)), bind: f.slice(14, 22), rest: f.slice(22) };
}

const same = (a: Uint8Array, b: Uint8Array) => a.length === b.length && a.every((x, i) => x === b[i]);

export function authServerMetadata(origin: string) {
  return {
    issuer: origin,
    authorization_endpoint: `${origin}/oauth/authorize`,
    token_endpoint: `${origin}/oauth/token`,
    registration_endpoint: `${origin}/oauth/register`,
    revocation_endpoint: `${origin}/oauth/revoke`,
    response_types_supported: ["code"],
    grant_types_supported: ["authorization_code", "refresh_token"],
    code_challenge_methods_supported: ["S256"],
    token_endpoint_auth_methods_supported: ["client_secret_post", "client_secret_basic", "none"],
    scopes_supported: [SCOPE, "offline_access"],
  };
}

export function protectedResourceMetadata(origin: string, id: string) {
  return {
    resource: `${origin}/${id}/mcp`,
    authorization_servers: [origin],
    scopes_supported: [SCOPE],
    bearer_methods_supported: ["header"],
    resource_name: "aiva",
  };
}

function isLoopback(host: string) {
  return host === "localhost" || host === "127.0.0.1" || host === "[::1]";
}

function validRedirectUri(uri: string): boolean {
  let u: URL;
  try {
    u = new URL(uri);
  } catch {
    return false;
  }
  if (u.hash) return false;
  if (u.protocol === "https:") return true;
  if (u.protocol === "http:") return isLoopback(u.hostname);
  // Native apps (cursor://, vscode://) may use private schemes; never script-like ones.
  return /^[a-z][a-z0-9+.-]*:$/.test(u.protocol) && !["javascript:", "data:", "file:", "vbscript:", "about:", "blob:"].includes(u.protocol);
}

/** Exact match, except loopback redirects match on any port (RFC 8252; Claude Code, Cursor). */
function redirectAllowed(registered: string[], given: string): boolean {
  if (registered.includes(given)) return true;
  try {
    const g = new URL(given);
    if (g.protocol !== "http:" || !isLoopback(g.hostname)) return false;
    return registered.some((r) => {
      const u = new URL(r);
      return u.protocol === "http:" && u.hostname === g.hostname && u.pathname === g.pathname && u.search === g.search;
    });
  } catch {
    return false;
  }
}

function oauthError(error: string, description: string, status = 400): Response {
  return json({ error, error_description: description }, status);
}

// ---- Dynamic client registration (the client_id itself carries the registration) -----------

export async function register(req: Request, env: Env): Promise<Response> {
  const body = (await req.json().catch(() => null)) as Record<string, unknown> | null;
  if (!body) return oauthError("invalid_client_metadata", "Expected a JSON body.");
  const uris = body.redirect_uris;
  if (!Array.isArray(uris) || uris.length === 0 || uris.length > 10 || !uris.every((u) => typeof u === "string" && u.length < 1000 && validRedirectUri(u))) {
    return oauthError("invalid_redirect_uri", "redirect_uris must be 1-10 https (or loopback http) URLs.");
  }
  const method = typeof body.token_endpoint_auth_method === "string" ? body.token_endpoint_auth_method : "client_secret_basic";
  if (!["none", "client_secret_post", "client_secret_basic"].includes(method)) {
    return oauthError("invalid_client_metadata", `Unsupported token_endpoint_auth_method ${method}.`);
  }
  const name = typeof body.client_name === "string" ? body.client_name.slice(0, 80) : null;
  const clientId = `aiva_client_${await packSigned(env, CLIENT, new TextEncoder().encode(JSON.stringify({ r: uris, n: name, m: method })))}`;
  return json(
    {
      client_id: clientId,
      ...(method === "none" ? {} : { client_secret: await derive(env, clientId), client_secret_expires_at: 0 }),
      client_id_issued_at: nowSec(),
      client_name: name ?? undefined,
      redirect_uris: uris,
      token_endpoint_auth_method: method,
      grant_types: ["authorization_code", "refresh_token"],
      response_types: ["code"],
      scope: SCOPE,
    },
    201,
  );
}

async function getClient(env: Env, clientId: string | null | undefined): Promise<Client | null> {
  if (!clientId?.startsWith("aiva_client_")) return null;
  // Up to 10 redirect URIs of up to 1000 characters each (see register); Gemini's alone makes
  // the id ~240 characters.
  const fields = await unpackSigned(env, clientId.slice(12), CLIENT, 16_000);
  if (!fields) return null;
  try {
    const c = JSON.parse(new TextDecoder().decode(fields)) as Omit<Client, "t">;
    return { t: "client", ...c };
  } catch {
    return null;
  }
}


function appLabel(client: Client, redirectUri: string): string {
  let host = "unknown";
  try {
    host = new URL(redirectUri).host || new URL(redirectUri).protocol.replace(":", "");
  } catch {}
  return `${client.n ?? "An app"} (${host})`;
}

// ---- Authorization: the page where the person pastes their aiva token ------------------------

export async function authorize(req: Request, env: Env, origin: string, showError: (title: string, body: string) => Response): Promise<Response> {
  const p = new URL(req.url).searchParams;
  const clientId = p.get("client_id");
  const client = await getClient(env, clientId);
  if (!client) return showError("Unknown app", "This connect link came from an app that isn't registered here. Remove the connector in your app and add it again.");
  const redirectUri = p.get("redirect_uri") ?? (client.r.length === 1 ? client.r[0] : "");
  if (!redirectAllowed(client.r, redirectUri)) return showError("Invalid redirect", "The app asked to return to an address it didn't register.");
  const state = p.get("state");
  const fail = (error: string, description: string) => redirect(withParams(redirectUri, { error, error_description: description, state }));

  if (p.get("response_type") !== "code") return fail("unsupported_response_type", "Only the authorization code flow is supported.");
  const challenge = p.get("code_challenge");
  if (!challenge || p.get("code_challenge_method") !== "S256") return fail("invalid_request", "PKCE with S256 is required.");

  let id: string | null = null;
  const resource = p.get("resource");
  if (resource) {
    try {
      id = new URL(resource).pathname.match(/^\/([a-f0-9]{20})\/mcp\/?$/)?.[1] ?? null;
    } catch {}
    if (!id) return fail("invalid_target", "Unknown resource. Use the URL shown by `aiva setup`.");
  }
  const pending = await sign(env, { t: "req", c: clientId!, r: redirectUri, s: state, cc: challenge, id, exp: nowSec() + REQUEST_TTL } satisfies PendingAuth);
  return redirect(`/oauth/connect?req=${encodeURIComponent(pending)}`);
}

/** GET /oauth/connect: the paste-your-token page for a pending authorization. */
export async function connect(req: Request, env: Env, origin: string, showError: (title: string, body: string) => Response): Promise<Response> {
  const raw = new URL(req.url).searchParams.get("req");
  const pending = await verify<PendingAuth>(env, raw, "req");
  if (!pending) return showError("This link has expired", "Go back to your app and click Connect again.");
  const client = await getClient(env, pending.c);
  if (!client) return showError("Unknown app", "Remove the connector in your app and add it again.");
  return connectPage({ pending: raw!, app: appLabel(client, pending.r), url: pending.id ? `${origin}/${pending.id}/mcp` : null });
}

/** POST /oauth/approve: checks the pasted token against the connector URL and returns to the app. */
export async function approve(req: Request, env: Env, origin: string, showError: (title: string, body: string) => Response): Promise<Response> {
  const body = await readBody(req);
  const pending = await verify<PendingAuth>(env, body.pending, "req");
  if (!pending) return showError("This link has expired", "Go back to your app and click Connect again.");
  const client = await getClient(env, pending.c);
  if (!client) return showError("Unknown app", "Remove the connector in your app and add it again.");

  if (body.action === "deny") {
    return redirect(withParams(pending.r, { error: "access_denied", error_description: "The user declined.", state: pending.s, iss: origin }));
  }
  const key = (body.token ?? "").trim();
  const again = (error: string) => connectPage({ pending: body.pending!, app: appLabel(client, pending.r), url: pending.id ? `${origin}/${pending.id}/mcp` : null, error });
  if (!looksLikeKey(key)) return again("That doesn't look like an aiva token. It starts with aiva_ — run `aiva token` on your computer to see it.");
  const id = await idFromKey(key);
  if (pending.id && pending.id !== id) return again("This token belongs to a different connector URL. Run `aiva token` on the computer whose URL you added.");

  let challenge: Uint8Array;
  try {
    challenge = fromB64url(pending.cc).slice(0, 12);
  } catch {
    return showError("Invalid request", "The app sent a malformed PKCE challenge.");
  }
  const code = `aiva_code_${await packGrant(env, CODE, CODE_TTL, id, await binding(pending.c, pending.r), challenge)}`;
  return redirect(withParams(pending.r, { code, state: pending.s, iss: origin }));
}

// ---- Token endpoint -----------------------------------------------------------------------

async function authenticateClient(req: Request, body: Record<string, string>, env: Env): Promise<{ id: string; client: Client } | Response> {
  let id = body.client_id;
  let secret = body.client_secret;
  const auth = req.headers.get("Authorization");
  if (auth?.startsWith("Basic ")) {
    try {
      const [u, s] = atob(auth.slice(6)).split(":");
      id = decodeURIComponent(u);
      secret = decodeURIComponent(s ?? "");
    } catch {
      return oauthError("invalid_client", "Malformed Basic credentials.", 401);
    }
  }
  const client = await getClient(env, id);
  // 401 invalid_client makes Claude register again (e.g. after the signing key changed).
  if (!client) return oauthError("invalid_client", "Unknown client.", 401);
  if (client.m !== "none" && secret !== (await derive(env, id!))) return oauthError("invalid_client", "Client authentication failed.", 401);
  return { id: id!, client };
}

async function issueTokens(env: Env, id: string, clientId: string): Promise<Response> {
  const bind = await binding(clientId);
  return json({
    access_token: `aiva_at_${await packGrant(env, ACCESS, ACCESS_TTL, id, bind)}`,
    token_type: "Bearer",
    expires_in: ACCESS_TTL,
    refresh_token: `aiva_rt_${await packGrant(env, REFRESH, REFRESH_TTL, id, bind)}`,
    scope: SCOPE,
  });
}

export async function token(req: Request, env: Env): Promise<Response> {
  const body = await readBody(req);
  const auth = await authenticateClient(req, body, env);
  if (auth instanceof Response) return auth;

  if (body.grant_type === "authorization_code") {
    const code = await unpackGrant(env, body.code?.replace(/^aiva_code_/, ""), CODE);
    if (!code) return oauthError("invalid_grant", "The code is invalid or expired.");
    // The binding covers client id + redirect URI; RFC 6749 requires the same redirect_uri here.
    if (!body.redirect_uri || !same(code.bind, await binding(auth.id, body.redirect_uri))) {
      return oauthError("invalid_grant", "The code was issued to a different app or redirect_uri.");
    }
    if (!/^[A-Za-z0-9._~-]{43,128}$/.test(body.code_verifier ?? "") || !same((await sha256Bytes(body.code_verifier)).slice(0, 12), code.rest)) {
      return oauthError("invalid_grant", "PKCE verification failed.");
    }
    return issueTokens(env, code.id, auth.id);
  }

  if (body.grant_type === "refresh_token") {
    const old = await unpackGrant(env, body.refresh_token?.replace(/^aiva_rt_/, ""), REFRESH);
    if (!old || !same(old.bind, await binding(auth.id))) return oauthError("invalid_grant", "The refresh token is invalid or expired.");
    return issueTokens(env, old.id, auth.id);
  }

  return oauthError("unsupported_grant_type", "Use authorization_code or refresh_token.");
}

/** Nothing is stored, so there's nothing to revoke per app; `aiva rotate` cuts off every app. */
export function revoke(): Response {
  return new Response(null, { status: 200 });
}

/** The computer id an access token grants, or null. */
export async function validateAccessToken(env: Env, bearer: string): Promise<string | null> {
  return (await unpackGrant(env, bearer.replace(/^aiva_at_/, ""), ACCESS))?.id ?? null;
}
