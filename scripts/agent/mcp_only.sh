#!/bin/sh
# An agent that reaches ograph only through MCP: pi with its built-in MCP client, ograph's tools
# and resources, and no file, shell or edit tools. The task is the server's own `investigate`
# prompt, since pi does not read MCP prompts. Writes WORKSPACE/.pi/mcp.json when it is missing
# and trusts the project for this run only (--approve). Extra arguments go to pi, such as
# --model.
#
# Usage: scripts/agent/mcp_only.sh WORKSPACE INCIDENT [pi args...]
# Needs: ograph on PATH (or OGRAPH=/path/to/ograph), pi logged in, python3.
set -eu
ws="$(cd "$1" && pwd)"; incident="$2"; shift 2
bin="$(command -v "${OGRAPH:-ograph}")"
if [ ! -f "$ws/.pi/mcp.json" ]; then
  mkdir -p "$ws/.pi"
  python3 -c 'import json,sys; print(json.dumps({"mcpServers": {"ograph": {"command": sys.argv[1], "args": ["serve", "--mcp"], "cwd": sys.argv[2], "exposure": "direct"}}}, indent=2))' "$bin" "$ws" > "$ws/.pi/mcp.json"
fi
task="$(printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"prompts/get\",\"params\":{\"name\":\"investigate\",\"arguments\":{\"incident\":\"$incident\"}}}" \
  | (cd "$ws" && "$bin" serve --mcp) \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["messages"][0]["content"]["text"])')"
cd "$ws"
exec pi -p --approve --no-session --no-extensions -e builtin:mcp --no-skills --no-context-files \
  --tools 'mcp__ograph__*,list_mcp_resources,list_mcp_resource_templates,read_mcp_resource' "$@" "$task"
