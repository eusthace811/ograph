// Host-free contract test for the generated pi extension: loads it, checks the registered
// tools and the prompt hook, and calls real tools through `ograph serve --mcp`. No model needed.
// Usage: node test_pi.mjs EXTENSION.ts DEMO_DIR OGRAPH_BIN
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { pathToFileURL } from "node:url";
const tools = new Map(), hooks = new Map();
const { default: extension } = await import(pathToFileURL(process.argv[2]).href);
extension({ registerTool(t) { tools.set(t.name, t); }, on(n, h) { hooks.set(n, h); } });
assert.deepEqual([...tools.keys()].sort(), ["ograph_before", "ograph_blast", "ograph_chains", "ograph_news", "ograph_similar", "ograph_sparql", "ograph_verdict", "ograph_why"]);
const prompt = await hooks.get("before_agent_start")({ systemPrompt: "base" });
assert.ok(prompt.systemPrompt.includes("ograph_before"));
const ctx = { cwd: process.argv[3] };
try {
  const chains = JSON.parse((await tools.get("ograph_chains").execute("1", { incident: "INC-5377" }, undefined, undefined, ctx)).content[0].text);
  assert.equal(chains.app, "checkout");
  assert.ok(chains.apps.length > 0);
  execFileSync(process.argv[4], ["harvest"], { cwd: ctx.cwd, stdio: "ignore" });
  const [blast, similar] = await Promise.all([
    tools.get("ograph_blast").execute("2", { app: "ledger-db" }, undefined, undefined, ctx),
    tools.get("ograph_similar").execute("3", { incident: "INC-5377" }, undefined, undefined, ctx),
  ]);
  assert.ok(JSON.parse(blast.content[0].text).processes.length > 0);
  assert.ok(Array.isArray(JSON.parse(similar.content[0].text)));
  await assert.rejects(() => tools.get("ograph_before").execute("4", { incident: "NOPE-1" }, undefined, undefined, ctx), /nothing in the graph/);
  await assert.rejects(() => tools.get("ograph_sparql").execute("5", { query: "DELETE WHERE { ?s ?p ?o }" }, undefined, undefined, ctx), /read-only/);
  for (const [typo, fix] of [["ops:dependOn", "ops:dependsOn"], ["inc:Incidnet", "inc:Incident"], ["evt:severty", "evt:severity"]]) {
    const q = typo.includes("Incid") ? `SELECT ?x WHERE { ?x a ${typo} }` : `SELECT ?x WHERE { ?x ${typo} ?y }`;
    await assert.rejects(() => tools.get("ograph_sparql").execute("7", { query: q }, undefined, undefined, ctx), new RegExp(`did you mean ${fix}\\?`));
  }
  await assert.rejects(() => tools.get("ograph_sparql").execute("9", { query: "SELECT ?e WHERE { ?e a evt:Event }" }, undefined, undefined, ctx), /VALUES \?type \{ evt:Alert /);
  const ok = JSON.parse((await tools.get("ograph_sparql").execute("8", { query: "SELECT ?a WHERE { ?a ops:dependsOn+ id:app\\/ledger-db }" }, undefined, undefined, ctx)).content[0].text);
  assert.ok(ok.rows.length > 0, "a correctly named query still answers");
  hooks.get("session_shutdown")();
  const again = JSON.parse((await tools.get("ograph_news").execute("6", {}, undefined, undefined, ctx)).content[0].text);
  assert.ok(again.length >= 1, "reconnects after shutdown");
  console.log("pi extension contract: PASS", { top: chains.apps[0].app, news: again.length });
} finally { hooks.get("session_shutdown")(); }
