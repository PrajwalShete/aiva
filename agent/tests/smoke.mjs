// End-to-end smoke test of an aiva build against a relay, the way an AI app uses it:
// setup → agent online → OAuth (register, paste token, code, token) → MCP tools/list + calls.
// With --service it also installs, stops, starts and removes the background service.
//
//   node tests/smoke.mjs <path-to-aiva-binary> [--service] [--server https://aiva.prajwalshete.com]
//
// Uses a throwaway AIVA_HOME and project folder; never touches the real install. Node 18+.

import { createHash, randomBytes } from "node:crypto";
import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, basename } from "node:path";

const args = process.argv.slice(2);
const AIVA = args.find((a) => !a.startsWith("--"));
const SERVICE = args.includes("--service");
const SERVER = args.includes("--server") ? args[args.indexOf("--server") + 1] : "https://aiva.prajwalshete.com";
const CALLBACK = "https://claude.ai/api/mcp/auth_callback";
if (SERVICE && !process.env.CI) {
  // The service is per user (one launchd label / Run entry), so this would replace a real install.
  console.error("--service replaces this user's real aiva service; it only runs in CI (set CI=1).");
  process.exit(2);
}
if (!AIVA) {
  console.error("usage: node tests/smoke.mjs <aiva binary> [--service] [--server URL]");
  process.exit(2);
}

let passed = 0;
const check = (cond, label, extra = "") => {
  if (!cond) {
    console.error(`FAIL  ${label}\n${typeof extra === "string" ? extra : JSON.stringify(extra, null, 2)}`);
    cleanup();
    process.exit(1);
  }
  passed++;
  console.log(`ok    ${label}`);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const project = mkdtempSync(join(tmpdir(), "aiva-smoke-"));
mkdirSync(join(project, "src"));
writeFileSync(join(project, "src", "hello.ts"), "export function greeting() {\n  return 'hello from aiva';\n}\n");
writeFileSync(join(project, ".env"), "SECRET=never-shown\n");
const home = mkdtempSync(join(tmpdir(), "aiva-smoke-home-"));
const env = { ...process.env, AIVA_HOME: home, AIVA_SERVER: SERVER };
const aiva = (...a) => execFileSync(AIVA, a, { env, stdio: ["ignore", "pipe", "pipe"], timeout: 60_000 }).toString();
let agent = null;
function cleanup() {
  try {
    if (SERVICE) aiva("service", "uninstall");
  } catch {}
  agent?.kill();
  rmSync(project, { recursive: true, force: true });
  rmSync(home, { recursive: true, force: true, maxRetries: 5, retryDelay: 500 });
}

// 1. Setup and the agent.
aiva("setup", "--folder", project, "--no-service");
const key = JSON.parse(readFileSync(join(home, "config.json"), "utf8")).key;
const id = createHash("sha256").update(key).digest("hex").slice(0, 20);
const url = `${SERVER}/${id}/mcp`;
check(/^aiva_[A-Za-z0-9_-]{43}$/.test(key), "setup made a key");

const online = async () => (await (await fetch(`${SERVER}/api/status`, { headers: { Authorization: `Bearer ${key}` } })).json()).online;
const waitOnline = async (want) => {
  for (let i = 0; i < 60 && (await online()) !== want; i++) await sleep(500);
  return (await online()) === want;
};
if (SERVICE) {
  aiva("service", "install");
  check(await waitOnline(true), "background service connects");
  const status = JSON.parse(aiva("status", "--json"));
  check(String(status.service).startsWith("running"), "status reports the service running", status);
} else {
  agent = spawn(AIVA, ["run"], { env, stdio: "ignore" });
  check(await waitOnline(true), "agent connects");
}

// 2. OAuth, like Claude: register, authorize, paste the token, exchange the code.
const form = (d) => ({ method: "POST", redirect: "manual", headers: { "Content-Type": "application/x-www-form-urlencoded" }, body: new URLSearchParams(d) });
const reg = await (await fetch(`${SERVER}/oauth/register`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ client_name: "smoke", redirect_uris: [CALLBACK], token_endpoint_auth_method: "client_secret_post" }) })).json();
const verifier = randomBytes(32).toString("base64url");
const challenge = createHash("sha256").update(verifier).digest("base64url");
let res = await fetch(`${SERVER}/oauth/authorize?` + new URLSearchParams({ response_type: "code", client_id: reg.client_id, redirect_uri: CALLBACK, code_challenge: challenge, code_challenge_method: "S256", state: "s", resource: url }), { redirect: "manual" });
res = await fetch(new URL(res.headers.get("Location"), SERVER));
const pending = (await res.text()).match(/name="pending" value="([^"]+)"/)[1];
res = await fetch(`${SERVER}/oauth/approve`, form({ pending, token: key, action: "allow" }));
const code = new URL(res.headers.get("Location")).searchParams.get("code");
const tokens = await (await fetch(`${SERVER}/oauth/token`, form({ grant_type: "authorization_code", code, redirect_uri: CALLBACK, code_verifier: verifier, client_id: reg.client_id, client_secret: reg.client_secret }))).json();
check(tokens.access_token, "OAuth flow returns an access token");

// 3. MCP.
let rpcId = 0;
const rpc = async (method, params = {}) => {
  const r = await fetch(url, {
    method: "POST",
    headers: { Authorization: `Bearer ${tokens.access_token}`, "Content-Type": "application/json", Accept: "application/json, text/event-stream", "MCP-Protocol-Version": "2025-06-18" },
    body: JSON.stringify({ jsonrpc: "2.0", id: ++rpcId, method, params }),
  });
  return r.json();
};
const call = async (name, args = {}) => (await rpc("tools/call", { name, arguments: args })).result;
const text = (r) => r?.content?.map((c) => c.text).join("\n") ?? "";

check((await rpc("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "smoke", version: "1" } })).result?.serverInfo, "initialize");
const tools = (await rpc("tools/list")).result?.tools ?? [];
check(tools.length >= 11, `tools/list returns the read tools (${tools.length})`, tools.map((t) => t.name));
const folder = basename(project);
check(text(await call("list_folders")).includes(folder), "list_folders shows the shared folder");
let r = await call("read_files", { files: [{ path: `${folder}/src/hello.ts` }] });
check(!r.isError && text(r).includes("hello from aiva"), "read_files reads a file", r);
r = await call("read_files", { files: [{ path: `${folder}\\src\\hello.ts` }] });
check(!r.isError && text(r).includes("hello from aiva"), "backslash paths work too", r);
r = await call("search", { pattern: "greeting", path: folder });
check(!r.isError && text(r).includes("hello.ts"), "search finds code", r);
r = await call("read_files", { files: [{ path: `${folder}/.env` }] });
check(!text(r).includes("never-shown"), "secret files stay hidden", r);
r = await call("project_overview", { path: folder });
check(!r.isError && text(r).includes("hello.ts"), "project_overview works", r);
r = await call("outline", { path: `${folder}/src/hello.ts` });
check(!r.isError && text(r).includes("greeting"), "outline parses the file", r);
r = await call("run", { command: "grep -n hello src/hello.ts" });
check(!r.isError && text(r).includes("hello from aiva"), "run executes a read-only command", r);

// 4. Service lifecycle.
if (SERVICE) {
  aiva("stop");
  check(await waitOnline(false), "aiva stop takes it offline");
  aiva("start");
  check(await waitOnline(true), "aiva start brings it back");
  aiva("restart");
  check(await waitOnline(true), "aiva restart keeps it online");
  aiva("service", "uninstall");
  check(await waitOnline(false), "service uninstall stops it");
  check(aiva("status", "--json").includes("not installed"), "status says not installed");
}

cleanup();
console.log(`\nAll ${passed} checks passed.`);
process.exit(0);
