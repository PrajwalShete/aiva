// aiva relay: a Cloudflare Worker that MCP apps (Claude, ChatGPT, Cursor...) talk to, and that
// each user's computer connects out to over a WebSocket. It stores nothing: computers are
// identified by a key they generate themselves, and OAuth state is signed, not saved.

import { handleMcp } from "./mcp";
import * as oauth from "./oauth";
import * as pages from "./pages";
import { ID_PATTERN, idFromKey, json, looksLikeKey, origin as originOf, preflight, redirect, text } from "./util";
import installScript from "./install.sh";
import installPowerShell from "./install.ps1";

export { AgentHub } from "./hub";

const ASSETS = /^(aiva-(darwin|linux)-(arm64|x64)|aivaw?-windows-(arm64|x64)\.exe)(\.sha256)?$/;
const MCP_PATH = new RegExp(`^/(${ID_PATTERN})/mcp/?$`);
const PRM_PATH = new RegExp(`^/\\.well-known/oauth-protected-resource/(${ID_PATTERN})/mcp/?$`);

export default {
  async fetch(req: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    const url = new URL(req.url);
    const path = url.pathname;
    const origin = originOf(env, req);
    if (req.method === "OPTIONS") return preflight();

    try {
      // MCP endpoint and OAuth discovery.
      let m = path.match(MCP_PATH);
      if (m) return handleMcp(req, env, m[1], origin);
      m = path.match(PRM_PATH);
      if (m) return json(oauth.protectedResourceMetadata(origin, m[1]));
      if (path === "/.well-known/oauth-authorization-server") return json(oauth.authServerMetadata(origin));

      if (path === "/oauth/register" && req.method === "POST") return oauth.register(req, env);
      if (path === "/oauth/authorize") return oauth.authorize(req, env, origin, pages.message);
      if (path === "/oauth/connect") return oauth.connect(req, env, origin, pages.message);
      if (path === "/oauth/approve" && req.method === "POST") return oauth.approve(req, env, origin, pages.message);
      if (path === "/oauth/token" && req.method === "POST") return oauth.token(req, env);
      if (path === "/oauth/revoke" && req.method === "POST") return oauth.revoke();

      // The agent on each computer.
      if (path === "/agent/connect") return agentConnect(req, env, origin);
      if (path === "/api/status") return status(req, env);
      if (path === "/api/version") return json({ version: env.LATEST_VERSION });

      // Installer and downloads.
      if (path === "/install.sh") {
        const script = installScript.replaceAll("__AIVA_SERVER__", origin).replaceAll("__AIVA_VERSION__", env.LATEST_VERSION);
        return text(script, 200, { "Content-Type": "text/x-shellscript; charset=utf-8", "Cache-Control": "public, max-age=300" });
      }
      if (path === "/install.ps1") {
        const script = installPowerShell.replaceAll("__AIVA_SERVER__", origin).replaceAll("__AIVA_VERSION__", env.LATEST_VERSION);
        return text(script, 200, { "Content-Type": "text/plain; charset=utf-8", "Cache-Control": "public, max-age=300" });
      }
      m = path.match(/^\/dl\/([^/]+)\/([^/]+)$/);
      if (m && ASSETS.test(m[2])) {
        const version = m[1] === "latest" ? env.LATEST_VERSION : m[1].replace(/^v/, "");
        if (!/^\d+\.\d+\.\d+([-.][a-z0-9.]+)?$/.test(version)) return text("Unknown version", 404);
        return download(`${env.RELEASES_URL}/download/v${version}/${m[2]}`, ctx);
      }

      if (path === "/health") return text("ok");
      if (path === "/") return pages.home(origin);
      return text("Not found", 404);
    } catch (err) {
      console.error("unhandled", path, err);
      return text("Something went wrong on our side. Please try again.", 500);
    }
  },
} satisfies ExportedHandler<Env>;

async function keyFrom(req: Request): Promise<string | null> {
  const auth = req.headers.get("Authorization");
  const key = auth?.startsWith("Bearer ") ? auth.slice(7).trim() : "";
  return looksLikeKey(key) ? idFromKey(key) : null;
}

/**
 * Serves a release file through Cloudflare (cached at the edge) instead of redirecting the
 * installer to GitHub's download servers, whose connections can stall on some networks.
 */
async function download(source: string, ctx: ExecutionContext): Promise<Response> {
  const cache = caches.default;
  const key = new Request(source);
  const hit = await cache.match(key);
  if (hit) return hit;
  const upstream = await fetch(source, { redirect: "follow", cf: { cacheTtl: 86400, cacheEverything: true } });
  if (!upstream.ok) return text(`Download not found (${upstream.status}).`, 404);
  const res = new Response(upstream.body, {
    headers: {
      "Content-Type": source.endsWith(".sha256") ? "text/plain; charset=utf-8" : "application/octet-stream",
      "Cache-Control": "public, max-age=86400, immutable",
      ...(upstream.headers.get("content-length") ? { "Content-Length": upstream.headers.get("content-length")! } : {}),
    },
  });
  ctx.waitUntil(cache.put(key, res.clone()));
  return res;
}

/** Whoever holds a key owns its id; route the WebSocket to that id's AgentHub. */
async function agentConnect(req: Request, env: Env, origin: string): Promise<Response> {
  if (req.headers.get("Upgrade")?.toLowerCase() !== "websocket") return text("Expected a WebSocket upgrade", 426);
  const id = await keyFrom(req);
  if (!id) return text("Missing or malformed aiva key. Run `aiva setup`.", 401);
  const headers = new Headers(req.headers);
  headers.set("X-Aiva-Id", id);
  headers.set("X-Aiva-Origin", origin);
  headers.delete("Authorization");
  return env.AGENTS.get(env.AGENTS.idFromName(id)).fetch(new Request(req.url, { method: "GET", headers }));
}

/** GET /api/status with the computer's key: whether the relay currently sees it online. */
async function status(req: Request, env: Env): Promise<Response> {
  const id = await keyFrom(req);
  if (!id) return json({ error: "Send the aiva key as a Bearer token." }, 401);
  return json({ id, ...(await env.AGENTS.get(env.AGENTS.idFromName(id)).status()) });
}
