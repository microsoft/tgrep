// Installed with absolute executable/config paths substituted by install.py.
// No pi SDK import is necessary: tools use the MCP server's JSON schemas.
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

const python = __PYTHON__;
const runtime = __RUNTIME__;
const config = __CONFIG__;
const tools = __TOOLS__;
const instructions = __INSTRUCTIONS__;

export default function (pi: any) {
  let child: ReturnType<typeof spawn> | undefined;
  let starting: Promise<void> | undefined;
  let sequence = 0;
  const pending = new Map<number, { resolve: (v: any) => void; reject: (e: Error) => void }>();

  function stop() {
    child?.kill();
    child = undefined;
    starting = undefined;
    for (const waiter of pending.values()) waiter.reject(new Error("tgrep MCP disconnected"));
    pending.clear();
  }

  function cancelled(signal?: AbortSignal): Promise<never> {
    return new Promise((_, reject) => {
      if (!signal) return;
      if (signal.aborted) return reject(new Error("Cancelled"));
      signal.addEventListener("abort", () => reject(new Error("Cancelled")), { once: true });
    });
  }

  function request(method: string, params: any, signal?: AbortSignal): Promise<any> {
    const id = ++sequence;
    return new Promise((resolve, reject) => {
      if (signal?.aborted) return reject(new Error("Cancelled"));
      const connection = child;
      if (!connection || connection.exitCode !== null || connection.signalCode !== null ||
          !connection.stdin || connection.stdin.destroyed || !connection.stdin.writable) {
        stop();
        return reject(new Error("tgrep MCP disconnected; retry to reconnect"));
      }
      const timer = setTimeout(() => cancel("tgrep MCP timed out"), 40000);
      function cleanup() { clearTimeout(timer); signal?.removeEventListener("abort", abort); pending.delete(id); }
      function cancel(message: string) {
        cleanup();
        try {
          if (connection.stdin?.writable && !connection.stdin.destroyed) {
            connection.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/cancelled", params: { requestId: id } }) + "\n");
          }
        } catch { /* Disconnect must not prevent cancellation from settling. */ }
        reject(new Error(message));
      }
      function abort() { cancel("Cancelled"); }
      signal?.addEventListener("abort", abort, { once: true });
      pending.set(id, {
        resolve(value) { cleanup(); resolve(value); },
        reject(error) { cleanup(); reject(error); },
      });
      try {
        connection.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n", (error) => {
          if (error) { cleanup(); reject(error); }
        });
      } catch (error) {
        cleanup();
        reject(error);
      }
    });
  }

  async function connect(cwd: string, signal?: AbortSignal) {
    if (signal?.aborted) throw new Error("Cancelled");
    // A request that starts while initialization is still running must not wait
    // out the whole startup timeout when its own signal aborts.
    if (starting) return signal ? Promise.race([starting, cancelled(signal)]) : starting;
    starting = (async () => {
      const process = spawn(python, [runtime, "mcp", "--config", config], { cwd, stdio: ["pipe", "pipe", "pipe"] });
      child = process;
      process.stderr?.on("data", () => {}); // Drain diagnostics; call errors are returned via MCP.
      process.stdin?.on("error", () => { if (child === process) stop(); });
      process.on("error", () => { if (child === process) stop(); });
      process.on("exit", () => { if (child === process) stop(); });
      createInterface({ input: process.stdout! }).on("line", (line) => {
        try {
          const reply = JSON.parse(line);
          const waiter = pending.get(reply.id);
          if (reply.error) waiter?.reject(new Error(reply.error.message));
          else waiter?.resolve(reply.result);
        } catch { stop(); }
      });
      const init = await request("initialize", {
        protocolVersion: "2025-11-25", capabilities: {}, clientInfo: { name: "tgrep-pi", version: "1.0.0" },
      }, signal);
      if (!init.capabilities?.tools) throw new Error("tgrep MCP has no tools capability");
      process.stdin?.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
    })();
    const attempt = starting;
    try { await attempt; } catch (error) {
      if (starting === attempt) stop();
      throw error;
    }
  }

  for (const tool of tools) {
    pi.registerTool({
      name: `tgrep_${tool.name}`,
      label: `tgrep ${tool.name}`,
      description: tool.description,
      parameters: tool.inputSchema,
      async execute(_id: string, args: any, signal: AbortSignal, _update: any, ctx: any) {
        await connect(ctx.cwd, signal);
        const result = await request("tools/call", { name: tool.name, arguments: args }, signal);
        if (result.isError) throw new Error(result.content.map((c: any) => c.text || "").join("\n"));
        return { content: result.content, details: result.structuredContent };
      },
    });
  }
  pi.on("session_start", async (_event: any, ctx: any) => {
    // Startup never waits for index construction. The MCP query path also ensures service availability.
    const warmup = spawn(python, [runtime, "ensure", "--config", config], { cwd: ctx.cwd, stdio: "ignore" });
    warmup.on("error", () => {});
    warmup.unref();
  });
  pi.on("before_agent_start", async (event: any) => ({
    systemPrompt: event.systemPrompt + "\n\n" + instructions.replace(/search_code/g, "tgrep_search_code").replace(/find_files/g, "tgrep_find_files"),
  }));
  pi.on("session_shutdown", stop);
}
