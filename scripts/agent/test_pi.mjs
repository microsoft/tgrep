// A host-free pi bridge contract test: loads the generated extension and calls
// its registered tools through a real MCP child. No model or API key required.
import assert from "node:assert/strict";
import { pathToFileURL } from "node:url";

const tools = new Map();
const hooks = new Map();
const { default: extension } = await import(pathToFileURL(process.argv[2]).href);
extension({
  registerTool(tool) { tools.set(tool.name, tool); },
  on(name, handler) { hooks.set(name, handler); },
});
assert.deepEqual([...tools.keys()], ["tgrep_search_code", "tgrep_find_files"]);
assert.ok(hooks.has("session_start"));
const prompt = await hooks.get("before_agent_start")({ systemPrompt: "original" });
assert.ok(prompt.systemPrompt.includes("tgrep_search_code"));
const ctx = { cwd: process.argv[3] };
try {
  const [matches, files] = await Promise.all([
    tools.get("tgrep_search_code").execute("1", { pattern: "needle", freshness: "current" }, undefined, undefined, ctx),
    tools.get("tgrep_find_files").execute("2", { pattern: "*.rs", freshness: "current" }, undefined, undefined, ctx),
  ]);
  assert.ok(matches.details.results.length > 0);
  assert.deepEqual(files.details.results, [{ path: "src/main.rs" }]);
  await assert.rejects(() => tools.get("tgrep_search_code").execute("3", { pattern: "[", literal: false, freshness: "current" }, undefined, undefined, ctx));
  // A disconnect after connect() returns but before execute() resumes must
  // reject immediately, not silently wait for the 40-second request timer.
  const started = Date.now();
  const racing = tools.get("tgrep_find_files").execute("4", { freshness: "current" }, undefined, undefined, ctx);
  hooks.get("session_shutdown")();
  await assert.rejects(racing, /disconnected/);
  assert.ok(Date.now() - started < 2000);
  const reconnected = await tools.get("tgrep_find_files").execute("5", { pattern: "*.rs", freshness: "current" }, undefined, undefined, ctx);
  assert.deepEqual(reconnected.details.results, [{ path: "src/main.rs" }]);
  // An already-aborted tool call must reject before spawning an adapter: the
  // missing working directory would otherwise surface as a spawn error.
  hooks.get("session_shutdown")();
  const aborted = new AbortController();
  aborted.abort();
  const abortedAt = Date.now();
  await assert.rejects(() => tools.get("tgrep_search_code").execute("6", { pattern: "needle", freshness: "current" }, aborted.signal, undefined, { cwd: process.argv[3] + "/missing" }), /Cancelled/);
  assert.ok(Date.now() - abortedAt < 2000);
  const afterAbort = await tools.get("tgrep_find_files").execute("7", { pattern: "*.rs", freshness: "current" }, undefined, undefined, ctx);
  assert.deepEqual(afterAbort.details.results, [{ path: "src/main.rs" }]);
  // A call that aborts while another call is still initializing must reject
  // immediately while the shared startup completes for the first caller.
  hooks.get("session_shutdown")();
  const startup = tools.get("tgrep_find_files").execute("8", { pattern: "*.rs", freshness: "current" }, undefined, undefined, ctx);
  const controller = new AbortController();
  const racingStartup = tools.get("tgrep_find_files").execute("9", { pattern: "*.rs", freshness: "current" }, controller.signal, undefined, ctx);
  controller.abort();
  await assert.rejects(racingStartup, /Cancelled/);
  assert.deepEqual((await startup).details.results, [{ path: "src/main.rs" }]);
} finally {
  hooks.get("session_shutdown")();
}
