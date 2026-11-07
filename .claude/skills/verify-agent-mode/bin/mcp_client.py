"""Runs on the box. An MCP host for one call to `solos-swarm mcp --operator verifier`.

python3 - <run> request <text> [<thread>]
python3 - <run> replies <thread> [<kind> <timeout_s>]   # with kind, waits until a reply of that kind arrives
python3 - <run> status
python3 - <run> memos [all]
python3 - <run> memo-add <text> [<scope>]
python3 - <run> memo-retire <memo> <reason>
"""
import json
import os
import subprocess
import sys
import time

run, op, rest = sys.argv[1], sys.argv[2], sys.argv[3:]
base = os.path.expanduser(f"~/work/verify/{run}")
server = subprocess.Popen(
    [f"{base}/bin/solos-swarm", "mcp", "--state-dir", f"{base}/state",
     "--operator", "verifier", "--config", f"{base}/agent.yaml"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
)
ids = iter(range(1, 1_000_000))


def send(method, params, notify=False):
    message = {"jsonrpc": "2.0", "method": method, "params": params}
    if not notify:
        message["id"] = next(ids)
    server.stdin.write(json.dumps(message) + "\n")
    server.stdin.flush()
    if notify:
        return None
    answer = json.loads(server.stdout.readline())
    if "error" in answer:
        sys.exit(f"MCP error: {answer['error']}")
    return answer["result"]


def call(tool, arguments):
    result = send("tools/call", {"name": tool, "arguments": arguments})
    text = "".join(part.get("text", "") for part in result.get("content", []))
    if result.get("isError"):
        sys.exit(f"{tool} refused: {text}")
    return json.loads(text)


send("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                    "clientInfo": {"name": "verify-agent-mode", "version": "1"}})
send("notifications/initialized", {}, notify=True)

if op == "request":
    arguments = {"text": rest[0]}
    if len(rest) > 1:
        arguments["thread"] = rest[1]
    print(json.dumps(call("agent_request", arguments)))
elif op == "replies":
    thread = rest[0]
    kind = rest[1] if len(rest) > 1 else None
    deadline = time.time() + (float(rest[2]) if len(rest) > 2 else 0)
    while True:
        replies = call("agent_replies", {"thread": thread})
        listed = replies if isinstance(replies, list) else replies.get("replies", [])
        if kind is None or any(r.get("kind") == kind for r in listed):
            print(json.dumps(replies, indent=1))
            break
        if time.time() > deadline:
            print(json.dumps(replies, indent=1))
            sys.exit(f"no '{kind}' reply on {thread} within {rest[2]} s")
        time.sleep(5)
elif op == "status":
    print(json.dumps(call("agent_status", {}), indent=1))
elif op == "memos":
    print(json.dumps(call("agent_memos", {"all": rest[:1] == ["all"]}), indent=1))
elif op == "memo-add":
    arguments = {"text": rest[0]}
    if len(rest) > 1:
        arguments["scope"] = rest[1]
    print(json.dumps(call("agent_memo_add", arguments), indent=1))
elif op == "memo-retire":
    print(json.dumps(call("agent_memo_retire", {"memo": rest[0], "reason": rest[1]}), indent=1))
else:
    sys.exit(f"unknown op {op}")
server.stdin.close()
server.wait(timeout=10)
