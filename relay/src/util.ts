// Small shared helpers: crypto, signed tokens, responses.

const encoder = new TextEncoder();

export function b64url(bytes: ArrayBuffer | Uint8Array): string {
  const arr = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  let s = "";
  for (const b of arr) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export function fromB64url(s: string): Uint8Array {
  const bin = atob(s.replace(/-/g, "+").replace(/_/g, "/"));
  return Uint8Array.from(bin, (c) => c.charCodeAt(0));
}

export async function sha256Hex(input: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", encoder.encode(input));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

export async function sha256B64url(input: string): Promise<string> {
  return b64url(await crypto.subtle.digest("SHA-256", encoder.encode(input)));
}

export const nowSec = () => Math.floor(Date.now() / 1000);

// ---- Computer keys ------------------------------------------------------------------------
// Each computer generates its own key (`aiva_` + 43 base64url chars) and never sends it
// anywhere except to prove itself. Its public id — the path in the connector URL — is the
// first 20 hex chars of SHA-256(key). Nothing about either is stored on the server.

export function looksLikeKey(key: string): boolean {
  return /^aiva_[A-Za-z0-9_-]{40,64}$/.test(key);
}

export async function idFromKey(key: string): Promise<string> {
  return (await sha256Hex(key)).slice(0, 20);
}

export const ID_PATTERN = "[a-f0-9]{20}";

// ---- Signed tokens --------------------------------------------------------------------------
// Everything the OAuth flow would normally store (clients, codes, tokens) is instead a payload
// signed with SIGNING_KEY: `<base64url json>.<base64url HMAC-SHA256>`.

let cachedKey: { secret: string; key: CryptoKey } | null = null;

async function hmacKey(env: Env): Promise<CryptoKey> {
  const secret = env.SIGNING_KEY;
  if (!secret || secret.length < 32) throw new Error("SIGNING_KEY is missing or too short (set it with `wrangler secret put SIGNING_KEY`).");
  if (cachedKey?.secret !== secret) {
    cachedKey = { secret, key: await crypto.subtle.importKey("raw", encoder.encode(secret), { name: "HMAC", hash: "SHA-256" }, false, ["sign", "verify"]) };
  }
  return cachedKey.key;
}

export async function sign(env: Env, payload: Record<string, unknown>): Promise<string> {
  const body = b64url(encoder.encode(JSON.stringify(payload)));
  const sig = await crypto.subtle.sign("HMAC", await hmacKey(env), encoder.encode(body));
  return `${body}.${b64url(sig)}`;
}

/** Verifies a signed token of the given type (`t`) that hasn't expired (`exp`, seconds). */
export async function verify<T extends Record<string, unknown>>(env: Env, token: string | null | undefined, type: string): Promise<T | null> {
  if (!token) return null;
  const dot = token.lastIndexOf(".");
  if (dot <= 0) return null;
  const body = token.slice(0, dot);
  try {
    const ok = await crypto.subtle.verify("HMAC", await hmacKey(env), fromB64url(token.slice(dot + 1)), encoder.encode(body));
    if (!ok) return null;
    const payload = JSON.parse(new TextDecoder().decode(fromB64url(body))) as T & { t?: string; exp?: number };
    if (payload.t !== type) return null;
    if (typeof payload.exp === "number" && payload.exp < nowSec()) return null;
    return payload;
  } catch {
    return null;
  }
}

// ---- Compact signed tokens ------------------------------------------------------------------
// Codes and tokens handed to apps are short binary records: type (1 byte) + fields + a 16-byte
// HMAC, base64url-encoded with no separators (~50-70 chars), like ordinary OAuth tokens.

async function mac16(env: Env, data: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.sign("HMAC", await hmacKey(env), data)).slice(0, 16);
}

export async function packSigned(env: Env, type: number, fields: Uint8Array<ArrayBufferLike>): Promise<string> {
  const body = new Uint8Array(1 + fields.length);
  body[0] = type;
  body.set(fields, 1);
  const mac = await mac16(env, body);
  const out = new Uint8Array(body.length + 16);
  out.set(body);
  out.set(mac, body.length);
  return b64url(out);
}

/** The fields of a compact token of `type` whose MAC checks out, or null. */
export async function unpackSigned(env: Env, token: string | null | undefined, type: number): Promise<Uint8Array | null> {
  if (!token || !/^[A-Za-z0-9_-]{24,200}$/.test(token)) return null;
  let raw: Uint8Array;
  try {
    raw = fromB64url(token);
  } catch {
    return null;
  }
  if (raw.length < 18 || raw[0] !== type) return null;
  const body = raw.slice(0, raw.length - 16);
  const expected = await mac16(env, body);
  const given = raw.slice(raw.length - 16);
  let diff = 0;
  for (let i = 0; i < 16; i++) diff |= expected[i] ^ given[i];
  return diff === 0 ? body.slice(1) : null;
}

export function u32(n: number): Uint8Array {
  return new Uint8Array([(n >>> 24) & 255, (n >>> 16) & 255, (n >>> 8) & 255, n & 255]);
}

export function readU32(b: Uint8Array, at: number): number {
  return ((b[at] << 24) | (b[at + 1] << 16) | (b[at + 2] << 8) | b[at + 3]) >>> 0;
}

export function hexToBytes(hex: string): Uint8Array {
  return Uint8Array.from(hex.match(/../g) ?? [], (h) => parseInt(h, 16));
}

export function bytesToHex(b: Uint8Array): string {
  return [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
}

export async function sha256Bytes(input: string): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.digest("SHA-256", encoder.encode(input)));
}

export function concat(...parts: Uint8Array<ArrayBufferLike>[]): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

/** A deterministic secret derived from the signing key (e.g. a client's client_secret). */
export async function derive(env: Env, label: string): Promise<string> {
  return b64url(await crypto.subtle.sign("HMAC", await hmacKey(env), encoder.encode(`derive:${label}`)));
}

// ---- Responses ----------------------------------------------------------------------------

const CORS: Record<string, string> = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, POST, DELETE, OPTIONS",
  "Access-Control-Allow-Headers": "Authorization, Content-Type, Mcp-Protocol-Version, Mcp-Session-Id, Last-Event-ID",
  "Access-Control-Expose-Headers": "WWW-Authenticate, Mcp-Session-Id",
  "Access-Control-Max-Age": "86400",
};

export function preflight(): Response {
  return new Response(null, { status: 204, headers: CORS });
}

export function json(data: unknown, status = 200, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "Content-Type": "application/json", "Cache-Control": "no-store", ...CORS, ...headers },
  });
}

export function text(body: string, status = 200, headers: Record<string, string> = {}): Response {
  return new Response(body, { status, headers: { "Content-Type": "text/plain; charset=utf-8", ...headers } });
}

export function html(body: string, nonce: string, status = 200): Response {
  return new Response(body, {
    status,
    headers: {
      "Content-Type": "text/html; charset=utf-8",
      "Cache-Control": "no-store",
      "X-Frame-Options": "DENY",
      "Referrer-Policy": "no-referrer",
      "X-Content-Type-Options": "nosniff",
      "Content-Security-Policy": `default-src 'self'; script-src 'nonce-${nonce}'; style-src 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'`,
    },
  });
}

export function redirect(location: string): Response {
  return new Response(null, { status: 302, headers: { Location: location, "Cache-Control": "no-store" } });
}

export function withParams(url: string, params: Record<string, string | null | undefined>): string {
  const u = new URL(url);
  for (const [k, v] of Object.entries(params)) if (v != null) u.searchParams.set(k, v);
  return u.toString();
}

export async function readBody(req: Request): Promise<Record<string, string>> {
  const type = req.headers.get("Content-Type") ?? "";
  if (type.includes("application/json")) {
    const data = (await req.json().catch(() => ({}))) as Record<string, unknown>;
    return Object.fromEntries(Object.entries(data).map(([k, v]) => [k, typeof v === "string" ? v : JSON.stringify(v)]));
  }
  const form = await req.formData().catch(() => null);
  const out: Record<string, string> = {};
  form?.forEach((v, k) => {
    if (typeof v === "string") out[k] = v;
  });
  return out;
}

export function esc(s: unknown): string {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
}

export function origin(env: Env, req: Request): string {
  return (env.PUBLIC_ORIGIN || new URL(req.url).origin).replace(/\/+$/, "");
}
