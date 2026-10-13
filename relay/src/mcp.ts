// Streamable HTTP MCP endpoint at /<name>/mcp, stateless: every POST carries JSON-RPC and gets
// a JSON response. initialize/ping are answered here; tools/list and tools/call go to the
// computer's AgentHub. The relay never interprets tool arguments or results.

import { errorResult, type ToolResult } from "./hub";
import { validateAccessToken } from "./oauth";
import { json } from "./util";

const SUPPORTED_VERSIONS = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const TOOL_TIMEOUT_MS = 60_000;

type RpcRequest = { jsonrpc?: string; id?: string | number | null; method?: string; params?: Record<string, unknown> };

type ToolInfo = { name?: string; annotations?: { readOnlyHint?: boolean } };

function instructions(name: string, canEdit: boolean): string {
  const lines = [
    `aiva gives you access to code folders on the user's computer so you can explore, explain${canEdit ? " and change" : ""} them.`,
    "Every path starts with a shared folder name (list_folders shows them and which allow edits), e.g. \"myapp/src/index.ts\".",
    "Work efficiently: start with project_overview, locate code with search, find_files, outline and find_symbol, then read_files only the line ranges you need. Call independent tools in parallel.",
    "The user is often talking to you by voice. When you explain, speak naturally: describe what the code does, how the pieces connect and why, instead of reading code, symbols or long paths aloud. Keep answers conversational and offer to go deeper.",
  ];
  if (canEdit) {
    lines.push(
      "Editing: read the code first. Before changing files, say in one or two sentences what you will change and get the user's go-ahead (they may be listening rather than watching). " +
        "Copy old_string exactly from what you read, with enough surrounding lines to be unique, and always write complete code: never placeholders such as \"// ... existing code ...\". " +
        "Each edit result shows the changed lines and any syntax errors; fix errors right away. Afterwards summarize what changed; every change can be reverted with the undo tool.",
    );
  } else {
    lines.push("Files can't be changed right now; the user can allow edits per folder with `aiva folders set <folder> write`.");
  }
  lines.push("Secret files such as .env and keys are never readable or writable.");
  return lines.join("\n");
}

export async function handleMcp(req: Request, env: Env, name: string, origin: string): Promise<Response> {
  const t0 = Date.now();
  const auth = req.headers.get("Authorization");
  const token = auth?.startsWith("Bearer ") ? auth.slice(7).trim() : null;
  const granted = token ? await validateAccessToken(env, token) : null;
  if (granted !== name) {
    const challenge = [`resource_metadata="${origin}/.well-known/oauth-protected-resource/${name}/mcp"`, `scope="mcp"`];
    if (token) challenge.unshift(`error="invalid_token"`);
    return json({ error: "unauthorized", error_description: "Connect this server from your MCP client to sign in." }, 401, {
      "WWW-Authenticate": `Bearer ${challenge.join(", ")}`,
    });
  }
  if (req.method !== "POST") return new Response(null, { status: 405, headers: { Allow: "POST" } });

  let body: unknown;
  try {
    body = await req.json();
  } catch {
    return json({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "Parse error" } }, 400);
  }
  const messages = (Array.isArray(body) ? body : [body]) as RpcRequest[];
  const requests = messages.filter((m) => m && typeof m.method === "string" && m.id !== undefined && m.id !== null);

  // Clients on a protocol era we don't speak get a 400 so they fall back to initialize.
  const version = req.headers.get("MCP-Protocol-Version");
  if (version && !SUPPORTED_VERSIONS.includes(version) && !requests.some((r) => r.method === "initialize")) {
    return new Response(null, { status: 400 });
  }
  if (requests.length === 0) return new Response(null, { status: 202 });

  const hub = env.AGENTS.get(env.AGENTS.idFromName(name));
  const tAuth = Date.now();
  const timings: Timing[] = [];
  const responses = await Promise.all(requests.map((r) => handleRequest(r, name, hub, timings)));
  const total = Date.now() - t0;
  // Server-Timing (ms): auth = token check; rpc = Worker→Durable Object call incl. everything below;
  // mac = Durable Object→computer→back; agent/tool = measured on the computer (µs precision).
  const t = timings[0];
  const parts = [`auth;dur=${tAuth - t0}`];
  if (t) {
    parts.push(`rpc;dur=${t.rpc_ms}`);
    if (t.call) {
      parts.push(`mac;dur=${t.call.do_ms}`);
      if (t.call.agent?.agent_us != null) parts.push(`agent;dur=${(t.call.agent.agent_us / 1000).toFixed(2)}`);
      if (t.call.agent?.tool_us != null) parts.push(`tool;dur=${(t.call.agent.tool_us / 1000).toFixed(2)}`);
    }
  }
  parts.push(`total;dur=${total}`);
  const cf = (req as Request & { cf?: { colo?: string } }).cf;
  console.log(JSON.stringify({ timing: true, method: requests.map((r) => r.method).join(","), tool: t?.tool, worker_colo: cf?.colo, do_colo: t?.call?.colo, total_ms: total, auth_ms: tAuth - t0, rpc_ms: t?.rpc_ms, mac_ms: t?.call?.do_ms, agent_us: t?.call?.agent?.agent_us, tool_us: t?.call?.agent?.tool_us, bytes: t?.call?.agent?.bytes }));
  return json(Array.isArray(body) ? responses : responses[0], 200, { "Server-Timing": parts.join(", "), "Access-Control-Expose-Headers": "WWW-Authenticate, Mcp-Session-Id, Server-Timing" });
}

type Timing = { tool?: string; rpc_ms: number; call?: import("./hub").CallTiming };

async function handleRequest(msg: RpcRequest, name: string, hub: DurableObjectStub<import("./hub").AgentHub>, timings: Timing[]) {
  const ok = (result: unknown) => ({ jsonrpc: "2.0", id: msg.id, result });
  const fail = (code: number, message: string) => ({ jsonrpc: "2.0", id: msg.id, error: { code, message } });
  switch (msg.method) {
    case "initialize": {
      const requested = msg.params?.protocolVersion as string | undefined;
      const tools = (await hub.tools()) as ToolInfo[];
      const canEdit = tools.some((t) => t.annotations?.readOnlyHint === false);
      return ok({
        protocolVersion: requested && SUPPORTED_VERSIONS.includes(requested) ? requested : SUPPORTED_VERSIONS[0],
        capabilities: { tools: { listChanged: false } },
        serverInfo: { name: "aiva", title: "aiva", version: "0.3.0" },
        instructions: instructions(name, canEdit),
      });
    }
    case "ping":
      return ok({});
    case "tools/list":
      return ok({ tools: await hub.tools() });
    case "tools/call": {
      const tool = msg.params?.name;
      if (typeof tool !== "string") return fail(-32602, "params.name is required");
      const started = Date.now();
      const res = await hub.call(tool, msg.params?.arguments ?? {}, TOOL_TIMEOUT_MS);
      timings.push({ tool, rpc_ms: Date.now() - started, call: res?.timing as import("./hub").CallTiming | undefined });
      if (res) return ok(res.result as ToolResult);
      return ok(
        errorResult(
          "The user's computer isn't connected right now. Ask them to make sure it's awake and online; running `aiva status` on it shows what's wrong.",
        ),
      );
    }
    default:
      return fail(-32601, `Method not found: ${msg.method}`);
  }
}

