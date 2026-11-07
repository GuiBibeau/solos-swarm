# Hand the agent work from an MCP host

`solos-swarm mcp` speaks the Model Context Protocol on stdin and stdout, so a host such as Claude Code or Codex starts
it as a stdio server. With `--state-dir` and `--operator`, it lets a person hand the agent work and read its replies.
How the protocol is served is in [The tool surface](../architecture.md#the-tool-surface).

## Give the agent an mcp source and channel

Name the operator in `policy.people` as `mcp:<name>`:

```yaml
policy:
  people: [mcp:alice]                  # the operator name the MCP host runs with
sources:
  - kind: mcp                          # reads <state dir>/mcp/inbox.jsonl, created at start
channels:
  - kind: mcp                          # writes <state dir>/mcp/outbox.jsonl
watchdog:                              # optional: alerts readable with agent_replies {thread: alerts}
  operator: {source: mcp, address: {operator: alice, thread: alerts}}
router: {kind: scripted}
executor: {kind: scripted}
budgets: {session_microusd: 500000}
```

Start `agent serve` once, so it creates the inbox. A request filed where no agent reads the inbox is refused instead of
lost.

## Add the server to the host

In Claude Code, add it to `.mcp.json` in a project, or run
`claude mcp add solos-agent -- solos-swarm mcp --state-dir /srv/agent/state --operator alice`:

```json
{
  "mcpServers": {
    "solos-agent": {
      "type": "stdio",
      "command": "/usr/local/bin/solos-swarm",
      "args": ["mcp", "--state-dir", "/srv/agent/state", "--operator", "alice",
               "--config", "/srv/agent/agent.yaml"]
    }
  }
}
```

In Codex, add it to `~/.codex/config.toml`, or run `codex mcp add solos-agent -- solos-swarm mcp --state-dir ...`:

```toml
[mcp_servers.solos-agent]
command = "/usr/local/bin/solos-swarm"
args = ["mcp", "--state-dir", "/srv/agent/state", "--operator", "alice", "--config", "/srv/agent/agent.yaml"]
```

`--config` only adds the Router's cap, the Watchdog's thresholds and a pause set in agent.yaml to `agent_status`.

## Use the tools

The host then has these tools:

| Tool | What it does |
|---|---|
| `agent_request {text, thread?}` | Files a request and returns its thread. A follow-up names the same thread. |
| `agent_replies {thread}` | Reads what the agent sent on the operator's own thread: the acknowledgement, any question, and the result. |
| `agent_status` | Returns the `agent status` report as JSON. Read-only. |
| `agent_memos` | Reads the agent's memos, as `agent memo list` does. |
| `agent_memo_add` | Adds a memo, as `agent memo add` does. |
| `agent_memo_retire` | Retires a memo, as `agent memo retire` does. |

The Policy's allowlist decides on each request, as it does for a Slack person. An operator who is not in
`policy.people` is ignored at the gate. Approvals still take Slack or Discord buttons.

This was checked with Claude Code 2.1.231 (`claude mcp list` shows it connected) and Codex 0.145.0, when the server
had three tools: both listed them and called `agent_request`.

## Serve one principal's tools

For a principal, such as a Claude Code worker, the process that admitted it starts
`solos-swarm mcp --socket <state dir>/broker.sock --token-file FILE` with a fresh token file. The token file is read
and deleted at start, and the host then lists and calls exactly that principal's tools. Here is a Router's exchange,
one JSON-RPC message per line:

```
> {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"host","version":"1"}}}
< {"id":1,"jsonrpc":"2.0","result":{"capabilities":{"tools":{"listChanged":false}},"instructions":"...","protocolVersion":"2025-06-18","serverInfo":{"name":"solos-swarm","version":"0.1.0"}}}
> {"jsonrpc":"2.0","method":"notifications/initialized"}
> {"jsonrpc":"2.0","id":2,"method":"tools/list"}
< {"id":2,"jsonrpc":"2.0","result":{"tools":[{"description":"...","inputSchema":{...},"name":"router_decide"},{"description":"...","inputSchema":{...},"name":"router_read"}]}}
> {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"router_read","arguments":{"id":"nope"}}}
< {"id":3,"jsonrpc":"2.0","result":{"content":[{"text":"no trigger, thread or ticket nope","type":"text"}],"isError":true}}
> {"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"run_code","arguments":{}}}
< {"error":{"code":-32602,"message":"Unknown tool: run_code"},"id":4,"jsonrpc":"2.0"}
```
