# Serving the graph to agents

Besides the CLI, two ways to reach the same answers. Both can run while a harvest runs.

## JSON-RPC over TCP, with automatic refresh

```bash
ograph serve --addr 127.0.0.1:7474 &
cat .ograph/serve.json                       # {"addr":"127.0.0.1:7474","pid":...}
printf '{"jsonrpc":"2.0","id":1,"method":"blast","params":{"app":"ledger-db"}}\n' | nc -w 2 127.0.0.1 7474
```

- One JSON request per line, one JSON response per line.
- Methods: `before {incident, window?}`, `chains {incident, rank?}`, `blast {app}`,
  `similar {incident}`, `code {incident, top?}`, `suspects {target, since?}`, `why {node}`,
  `uses {source}`, `news {since?}`, `sparql {query, limit?}`, `resolve {mentions[]}`, `cite_check {text}`,
  `verdict {chain, decision, evidence[], by}`.
- `code` and `suspects` run ogrep on each call, so they are slower than the graph-only methods.
- Every 5 s the server checks the files a harvest reads (for a `code` repository, its
  `.git/logs/HEAD`), plus `config.toml`, `ontology.ttl` and
  `shapes.ttl`, and re-harvests on any change.
- Without `--addr` it picks a free port; read it from `.ograph/serve.json`.

## MCP over stdio

```bash
ograph serve --mcp
```

The same thirteen tools, named `before`, `chains`, `blast`, `similar`, `code`, `suspects`, `why`, `uses`,
`news`, `sparql`, `resolve`, `cite_check` and `verdict`, for any MCP client. Every tool but
`verdict` carries `annotations.readOnlyHint: true`. There is no file watcher in this mode; run
`ograph harvest` to refresh (it works while the MCP server is open).

- Resources: `ograph://ontology` (the workspace's `ontology.ttl`), `ograph://incidents` (id,
  application, open time, title and status of each incident, newest first).
- Resource template: `ograph://incident/{id}` gives `before`, `chains` and `similar` for the
  incident and `blast` for its top-ranked application in one read.
- Prompt: `investigate {incident}` gives the order of the tools and the reporting rules. ograph
  builds it from `references/investigation.md`, so the two never differ.

Example client entry:

```json
{ "mcpServers": { "ograph": { "command": "ograph", "args": ["serve", "--mcp", "--root", "/path/to/workspace"] } } }
```

## pi

```bash
python3 scripts/agent/install.py --bin "$(which ograph)" --root /path/to/workspace --out ~/.pi/agent/extensions/ograph.ts
```

This writes a pi extension whose tools are `ograph_before`, `ograph_chains`, ... with the
investigation rules in its prompt. Each pi session starts one `ograph serve --mcp`.

## An agent with MCP only

```bash
scripts/agent/mcp_only.sh /path/to/workspace INC-5377
```

This runs pi with its built-in MCP client and no file, shell or edit tools: only ograph's tools
and its resources (`list_mcp_resources`, `read_mcp_resource`). pi does not read MCP prompts, so
the script passes the `investigate` prompt's text as the task. It writes
`.pi/mcp.json` in the workspace when it is missing and trusts the project for that run only.
