// ograph tools for pi. install.py substitutes __BIN__ and __TOOLS__ (the MCP tool list from
// `ograph serve --mcp`), so the extension needs no pi SDK import. Each session spawns one
// `ograph serve --mcp` in the session's working directory and reuses it.
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

const bin: string = __BIN__;
const tools: any[] = __TOOLS__;
const instructions: string = __INSTRUCTIONS__;

export default function (pi: any) {
  let child: ReturnType<typeof spawn> | undefined;
  let ready: Promise<void> | undefined;
  let sequence = 0;
  const pending = new Map<number, { resolve: (v: any) => void; reject: (e: Error) => void }>();

  function stop() {
    child?.kill();
    child = undefined;
    ready = undefined;
    for (const w of pending.values()) w.reject(new Error("ograph disconnected; retry to reconnect"));
    pending.clear();
  }

  function request(method: string, params: any, signal?: AbortSignal): Promise<any> {
    const id = ++sequence;
    return new Promise((resolve, reject) => {
      if (signal?.aborted) return reject(new Error("Cancelled"));
      const c = child;
      if (!c?.stdin?.writable) return reject(new Error("ograph disconnected; retry to reconnect"));
      const timer = setTimeout(() => { pending.delete(id); reject(new Error("ograph timed out")); }, 60000);
      const abort = () => { clearTimeout(timer); pending.delete(id); reject(new Error("Cancelled")); };
      signal?.addEventListener("abort", abort, { once: true });
      pending.set(id, {
        resolve(v) { clearTimeout(timer); signal?.removeEventListener("abort", abort); pending.delete(id); resolve(v); },
        reject(e) { clearTimeout(timer); signal?.removeEventListener("abort", abort); pending.delete(id); reject(e); },
      });
      c.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
    });
  }

  function connect(cwd: string, signal?: AbortSignal): Promise<void> {
    if (signal?.aborted) return Promise.reject(new Error("Cancelled"));
    if (ready) return ready;
    ready = (async () => {
      const p = spawn(bin, ["serve", "--mcp"], { cwd, stdio: ["pipe", "pipe", "pipe"] });
      child = p;
      p.stderr?.on("data", () => {});
      p.on("exit", () => { if (child === p) stop(); });
      p.on("error", () => { if (child === p) stop(); });
      createInterface({ input: p.stdout! }).on("line", (line) => {
        try {
          const r = JSON.parse(line);
          const w = pending.get(r.id);
          if (r.error) w?.reject(new Error(r.error.message)); else w?.resolve(r.result);
        } catch { stop(); }
      });
      const init = await request("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "ograph-pi", version: "1" } }, signal);
      if (!init?.capabilities?.tools) throw new Error("ograph MCP has no tools capability");
    })();
    ready.catch(() => stop());
    return ready;
  }

  for (const tool of tools) {
    pi.registerTool({
      name: `ograph_${tool.name}`,
      label: `ograph ${tool.name}`,
      description: tool.description,
      parameters: tool.inputSchema,
      async execute(_id: string, args: any, signal: AbortSignal, _update: any, ctx: any) {
        await connect(ctx.cwd, signal);
        const result = await request("tools/call", { name: tool.name, arguments: args }, signal);
        if (result.isError) throw new Error(result.content.map((c: any) => c.text || "").join("\n"));
        return { content: result.content, details: {} };
      },
    });
  }
  pi.on("before_agent_start", async (event: any) => ({ systemPrompt: event.systemPrompt + "\n\n" + instructions }));
  pi.on("session_shutdown", stop);
}
