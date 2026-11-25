// Host-free contract test for the generated pi extension and for every MCP method of
// `ograph serve --mcp`: loads the extension, checks the registered tools and the prompt hook,
// calls real tools through it, then speaks raw MCP to the server. No model needed.
// Usage: node test_pi.mjs EXTENSION.ts DEMO_DIR OGRAPH_BIN
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
const tools = new Map(), hooks = new Map();
const { default: extension } = await import(pathToFileURL(process.argv[2]).href);
extension({ registerTool(t) { tools.set(t.name, t); }, on(n, h) { hooks.set(n, h); } });
assert.deepEqual([...tools.keys()].sort(), ["ograph_before", "ograph_blast", "ograph_chains", "ograph_cite_check", "ograph_code", "ograph_news", "ograph_resolve", "ograph_similar", "ograph_sparql", "ograph_suspects", "ograph_verdict", "ograph_why"]);
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

const send = (msgs) => execFileSync(process.argv[4], ["serve", "--mcp"], { cwd: ctx.cwd, input: msgs.map((m, k) => JSON.stringify({ jsonrpc: "2.0", id: k + 1, ...m })).join("\n") + "\n" })
  .toString().trim().split("\n").map((l) => JSON.parse(l));
const [init, list, resources, templates, ontology, incidents, incident, badUri, prompts, investigate, noArg, ping, resolve, cite, unknown] = send([
  { method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "contract", version: "1" } } },
  { method: "tools/list" },
  { method: "resources/list" },
  { method: "resources/templates/list" },
  { method: "resources/read", params: { uri: "ograph://ontology" } },
  { method: "resources/read", params: { uri: "ograph://incidents" } },
  { method: "resources/read", params: { uri: "ograph://incident/INC-5377" } },
  { method: "resources/read", params: { uri: "ograph://incident/NOPE-1" } },
  { method: "prompts/list" },
  { method: "prompts/get", params: { name: "investigate", arguments: { incident: "INC-5377" } } },
  { method: "prompts/get", params: { name: "investigate", arguments: {} } },
  { method: "ping" },
  { method: "tools/call", params: { name: "resolve", arguments: { mentions: ["chekout", "ledger-service", "payments"] } } },
  { method: "tools/call", params: { name: "cite_check", arguments: { text: "1. ledger-db id:chain/inc-5377-a7370ff7d4358525 for INC-5377, not INC-9999 or id:chain/inc-5377-0123456789abcdef" } } },
  { method: "resources/subscribe", params: { uri: "ograph://incidents" } },
]);
assert.deepEqual(init.result.capabilities, { prompts: {}, resources: {}, tools: {} });
assert.deepEqual(list.result.tools.filter((t) => !t.annotations.readOnlyHint).map((t) => t.name), ["verdict"]);
assert.equal(list.result.tools.length, 12);
assert.deepEqual(resources.result.resources.map((r) => [r.uri, r.mimeType]), [["ograph://ontology", "text/turtle"], ["ograph://incidents", "application/json"]]);
assert.deepEqual(templates.result.resourceTemplates.map((r) => r.uriTemplate), ["ograph://incident/{id}"]);
assert.equal(ontology.result.contents[0].text, readFileSync(`${ctx.cwd}/.ograph/ontology.ttl`, "utf8"));
const inc = JSON.parse(incidents.result.contents[0].text);
assert.deepEqual(inc[0], { id: "INC-5377", app: "checkout", at: "2026-11-22T22:26:40Z", title: "Checkout latency and payment errors", status: "open" });
assert.deepEqual(inc.map((i) => i.id), ["INC-5377", "INC-4501", "INC-4388", "INC-4120"]);
const one = JSON.parse(incident.result.contents[0].text);
assert.deepEqual(Object.keys(one).sort(), ["before", "blast", "chains", "similar"]);
assert.equal(one.chains.apps[0].app, "ledger-db");
assert.equal(one.chains.apps[0].chain.id, "https://ograph.dev/id/chain/inc-5377-a7370ff7d4358525");
assert.equal(one.blast.app, "ledger-db");
assert.equal(one.similar[0].incident, "id:inc/inc-4120");
assert.match(badUri.error.message, /nothing in the graph is called NOPE-1/);
assert.deepEqual(prompts.result.prompts.map((p) => [p.name, p.arguments.map((a) => [a.name, a.required])]), [["investigate", [["incident", true]]]]);
const text = investigate.result.messages[0].content.text;
assert.ok(text.includes("## 1. What happened before it (tool `before`)") && text.indexOf("(tool `chains`)") > text.indexOf("(tool `before`)"));
assert.ok(text.includes("7. What the person must confirm before a verdict."));
assert.match(noArg.error.message, /missing argument incident/);
assert.deepEqual(ping.result, {});
assert.deepEqual(JSON.parse(resolve.result.content[0].text).results.map((r) => [r.mention, r.status, r.app]), [["chekout", "resolved", "id:app/checkout"], ["ledger-service", "ambiguous", null], ["payments", "resolved", "id:app/payments-api"]]);
assert.deepEqual(JSON.parse(cite.result.content[0].text).map((c) => [c.cited, c.status]), [["ledger-db", "present"], ["id:chain/inc-5377-a7370ff7d4358525", "present"], ["INC-5377", "present"], ["INC-9999", "unknown"], ["id:chain/inc-5377-0123456789abcdef", "unknown"]]);
assert.equal(unknown.error.code, -32601);
console.log("MCP contract: PASS", { incidents: inc.length, top: one.chains.apps[0].app, prompt_chars: text.length });
