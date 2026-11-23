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
  `similar {incident}`, `why {node}`, `news {}`, `sparql {query, limit?}`,
  `verdict {chain, decision, evidence[], by}`.
- Every 5 s the server checks the files a harvest reads, plus `config.toml`, `ontology.ttl` and
  `shapes.ttl`, and re-harvests on any change.
- Without `--addr` it picks a free port; read it from `.ograph/serve.json`.

## MCP over stdio

```bash
ograph serve --mcp
```

The same eight tools, named `before`, `chains`, `blast`, `similar`, `why`, `news`, `sparql`
and `verdict`, for any MCP client. There is no file watcher in this mode; run `ograph harvest`
to refresh (it works while the MCP server is open).

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
