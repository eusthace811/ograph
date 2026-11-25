#!/usr/bin/env node
// Lab page in headless Chrome over the DevTools protocol (no packages): counts JavaScript errors,
// clicks a chain arrow, a neighbourhood node and a copy button, checks that a phone-width
// viewport has no horizontal page scroll, and saves light, dark and phone screenshots.
// Usage: node lab_browser.mjs PAGE.html OUT_PREFIX
import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const [page, prefix] = process.argv.slice(2);
const chrome = process.env.CHROME ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const port = 9300 + Math.floor(Math.random() * 500);
const proc = spawn(chrome, ["--headless=new", `--remote-debugging-port=${port}`, `--user-data-dir=${mkdtempSync(join(tmpdir(), "lab-"))}`, "--no-first-run", "about:blank"], { stdio: "ignore" });
const wait = ms => new Promise(r => setTimeout(r, ms));
let ws;
for (let i = 0; i < 50 && !ws; i++) {
  await wait(200);
  try {
    const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
    const t = targets.find(t => t.type === "page");
    if (t) ws = new WebSocket(t.webSocketDebuggerUrl);
  } catch {}
}
await new Promise(r => ws.addEventListener("open", r));
let id = 0;
const pending = new Map(), errors = [];
ws.addEventListener("message", m => {
  const msg = JSON.parse(m.data);
  if (msg.id && pending.has(msg.id)) { pending.get(msg.id)(msg); pending.delete(msg.id); }
  if (msg.method === "Runtime.exceptionThrown") errors.push(msg.params.exceptionDetails.exception?.description ?? msg.params.exceptionDetails.text);
  if (msg.method === "Runtime.consoleAPICalled" && msg.params.type === "error") errors.push(msg.params.args.map(a => a.value).join(" "));
  if (msg.method === "Log.entryAdded" && msg.params.entry.level === "error") errors.push(msg.params.entry.text);
});
const send = (method, params = {}) => new Promise(r => { const i = ++id; pending.set(i, r); ws.send(JSON.stringify({ id: i, method, params })); });
const js = async expr => (await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true })).result.result.value;
const shot = async name => {
  const r = await send("Page.captureScreenshot", { format: "png", captureBeyondViewport: false });
  writeFileSync(`${prefix}-${name}.png`, Buffer.from(r.result.data, "base64"));
  return `${prefix}-${name}.png`;
};

await send("Runtime.enable"); await send("Log.enable"); await send("Page.enable");
await send("Emulation.setDeviceMetricsOverride", { width: 1100, height: 1400, deviceScaleFactor: 1, mobile: false });
const t0 = Date.now();
await send("Page.navigate", { url: "file://" + resolve(page) });
await js("new Promise(r => document.readyState === 'complete' ? r() : addEventListener('load', r))");
const loadMs = Date.now() - t0;
const out = { page, load_ms: loadMs };
out.sections = await js("document.querySelectorAll('section').length");
out.marks = await js("document.querySelectorAll('svg.timeline rect[data-ev]').length");
out.arrows = await js("document.querySelectorAll('svg.timeline path.hop').length");
out.nodes = await js("document.querySelectorAll('svg.hood g.node').length");
out.arrow_click = await js(`(() => { const h = document.querySelector('section path.hop'); if (!h) return 'no arrow';
  h.dispatchEvent(new MouseEvent('click', {bubbles: true})); const t = document.getElementById(h.dataset.open);
  return t.classList.contains('focus') && t.closest('details').open ? 'opened ' + h.dataset.open : 'not opened'; })()`);
out.arrow_shot = await shot("arrow");
out.filter_click = await js(`(() => { const sec = document.querySelector('section'); const n = [...sec.querySelectorAll('svg.hood g.node.has')].pop();
  n.dispatchEvent(new MouseEvent('click', {bubbles: true})); const lanes = [...sec.querySelectorAll('.lane')];
  const kept = lanes.filter(l => !l.classList.contains('dim')).map(l => l.dataset.app);
  const r = 'filtered to ' + n.dataset.app + ': ' + kept.length + ' of ' + lanes.length + ' lanes shown (' + kept.join(',') + ')';
  return r; })()`);
out.filter_shot = await shot("filter");
out.filter_clear = await js(`(() => { const sec = document.querySelector('section'); const n = [...sec.querySelectorAll('svg.hood g.node.has')].pop();
  n.dispatchEvent(new MouseEvent('click', {bubbles: true})); return sec.querySelectorAll('.lane.dim').length + ' lanes dimmed after a second click'; })()`);
out.copy_click = await js(`(() => { const b = document.querySelector('button[data-copy="reject"]'); if (!b) return 'no chain';
  b.click(); return b.parentElement.querySelector('code').textContent; })()`);
await wait(200);
await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
out.dark_bg = await js("getComputedStyle(document.body).backgroundColor");
await js("scrollTo(0, 0)");
out.dark_shot = await shot("dark");
await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "light" }] });
await send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 2, mobile: true });
await wait(300);
out.phone_scroll = await js("document.documentElement.scrollWidth + ' page width at a ' + innerWidth + ' px viewport'");
out.phone_shot = await shot("phone");
out.js_errors = errors;
console.log(JSON.stringify(out, null, 1));
proc.kill();
process.exit(errors.length ? 1 : 0);
