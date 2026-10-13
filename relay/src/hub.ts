// One AgentHub Durable Object per computer id. It only relays traffic: it holds that
// computer's WebSocket (hibernatable, so idle connections cost nothing) and turns MCP requests
// into messages to the agent and back. It stores nothing — even the tool list is asked live.

import { DurableObject } from "cloudflare:workers";

export type ToolResult = { content: Array<{ type: "text"; text: string }>; isError?: boolean };

type Attachment = { id: string; origin: string; connectedAt: number; ready: boolean; device?: string; version?: string };
type Pending = { resolve: (r: unknown, timing?: AgentTiming) => void; timer: ReturnType<typeof setTimeout> };
export type AgentTiming = { tool_us?: number; agent_us?: number; bytes?: number };
export type CallTiming = { do_ms: number; agent?: AgentTiming; colo?: string };

const AGENT_PROTOCOL = 2;
const CLOSE_REPLACED = 4001;
const CLOSE_BAD_HELLO = 4002;
/** The agent pings every 30s; treat it as gone after this long without one. */
const STALE_AFTER_MS = 90_000;

export function errorResult(text: string): ToolResult {
  return { content: [{ type: "text", text }], isError: true };
}

export class AgentHub extends DurableObject<Env> {
  private pending = new Map<number, Pending>();

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    // Answered by the runtime without waking the object.
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair('{"type":"ping"}', '{"type":"pong"}'));
  }

  /** WebSocket upgrade from the Worker, which already derived the id from the agent's key. */
  async fetch(request: Request): Promise<Response> {
    const id = request.headers.get("X-Aiva-Id");
    const origin = request.headers.get("X-Aiva-Origin");
    if (request.headers.get("Upgrade") !== "websocket" || !id || !origin) return new Response("Expected a WebSocket upgrade", { status: 426 });
    const { 0: client, 1: server } = new WebSocketPair();
    this.ctx.acceptWebSocket(server, ["agent"]);
    server.serializeAttachment({ id, origin, connectedAt: Date.now(), ready: false } satisfies Attachment);
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    let msg: { type?: string; [k: string]: unknown };
    try {
      msg = JSON.parse(typeof message === "string" ? message : new TextDecoder().decode(message));
    } catch {
      return;
    }
    const meta = ws.deserializeAttachment() as Attachment;

    if (msg.type === "hello") {
      if (msg.protocol !== AGENT_PROTOCOL) {
        ws.send(JSON.stringify({ type: "error", message: "This aiva version doesn't match the server. Run `aiva update`." }));
        ws.close(CLOSE_BAD_HELLO, "Unsupported protocol");
        return;
      }
      // One computer per key: the newest connection wins.
      for (const other of this.ctx.getWebSockets("agent")) {
        if (other !== ws) other.close(CLOSE_REPLACED, "Another computer connected with this key");
      }
      ws.serializeAttachment({ ...meta, ready: true, device: String(msg.device ?? "computer").slice(0, 80), version: String(msg.version ?? "?").slice(0, 20) } satisfies Attachment);
      ws.send(JSON.stringify({ type: "welcome", id: meta.id, url: `${meta.origin}/${meta.id}/mcp` }));
      return;
    }

    if (msg.type === "result" && typeof msg.id === "number") {
      const p = this.pending.get(msg.id);
      if (!p) return; // late reply after a timeout
      clearTimeout(p.timer);
      this.pending.delete(msg.id);
      p.resolve(msg.result, msg.timing as AgentTiming | undefined);
      return;
    }

    if (msg.type === "ping") ws.send('{"type":"pong"}');
  }

  async webSocketClose(ws: WebSocket, code: number, reason: string): Promise<void> {
    this.onGone(ws);
    try {
      ws.close(code === 1005 ? 1000 : code, reason);
    } catch {
      // already closed
    }
  }

  async webSocketError(ws: WebSocket): Promise<void> {
    this.onGone(ws);
  }

  private onGone(ws: WebSocket) {
    if (this.liveAgent(ws)) return;
    for (const [id, p] of this.pending) {
      clearTimeout(p.timer);
      p.resolve(null);
      this.pending.delete(id);
    }
  }

  /** The connected, greeted agent socket, if any (optionally ignoring one that is closing). */
  private liveAgent(except?: WebSocket): WebSocket | null {
    const now = Date.now();
    for (const ws of this.ctx.getWebSockets("agent")) {
      if (ws === except) continue;
      const meta = ws.deserializeAttachment() as Attachment | null;
      if (!meta?.ready) continue;
      const lastPing = this.ctx.getWebSocketAutoResponseTimestamp(ws)?.getTime() ?? meta.connectedAt;
      if (now - Math.max(lastPing, meta.connectedAt) > STALE_AFTER_MS) {
        ws.close(1001, "No heartbeat");
        continue;
      }
      return ws;
    }
    return null;
  }

  /** Sends a request to the agent and waits for its reply; null if offline or disconnected. */
  private request(message: Record<string, unknown>, timeoutMs: number, onTimeout: unknown, onTiming?: (t: CallTiming) => void): Promise<unknown> {
    const ws = this.liveAgent();
    if (!ws) return Promise.resolve(null);
    const sent = Date.now();
    let id: number;
    do id = Math.floor(Math.random() * 2 ** 48);
    while (this.pending.has(id));
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        resolve(onTimeout);
      }, timeoutMs);
      this.pending.set(id, {
        resolve: (r, agent) => {
          onTiming?.({ do_ms: Date.now() - sent, agent, colo: this.colo });
          resolve(r);
        },
        timer,
      });
      ws.send(JSON.stringify({ ...message, id }));
    });
  }

  // ---- RPC methods called by the Worker -------------------------------------------------

  /** Forwards a tool call. Returns null when the computer isn't connected. */
  /** Where this Durable Object runs (Cloudflare data-center code), looked up once. */
  private colo?: string;
  private async locate() {
    if (this.colo) return;
    try {
      const trace = await (await fetch("https://cloudflare.com/cdn-cgi/trace")).text();
      this.colo = trace.match(/colo=(\w+)/)?.[1];
    } catch {}
  }

  /** Forwards a tool call. Returns null when the computer isn't connected. */
  async call(tool: string, args: unknown, timeoutMs: number): Promise<{ result: ToolResult; timing?: CallTiming } | null> {
    await this.locate();
    let timing: CallTiming | undefined;
    const result = await this.request(
      { type: "call", tool, args: args ?? {} },
      timeoutMs,
      errorResult(`The computer didn't answer within ${Math.round(timeoutMs / 1000)}s. Try a narrower request.`),
      (t) => (timing = t),
    );
    if (result === null) return null;
    const r = result as ToolResult;
    return { result: Array.isArray(r?.content) ? r : errorResult("The computer sent a malformed result."), timing };
  }

  /** The tools the connected computer offers, or [] when it's offline. */
  async tools(): Promise<unknown[]> {
    const result = (await this.request({ type: "list" }, 10_000, null)) as { tools?: unknown[] } | null;
    return Array.isArray(result?.tools) ? result.tools : [];
  }

  async status(): Promise<{ online: boolean; device: string | null; version: string | null }> {
    const ws = this.liveAgent();
    const meta = ws?.deserializeAttachment() as Attachment | undefined;
    return { online: !!ws, device: meta?.device ?? null, version: meta?.version ?? null };
  }
}
