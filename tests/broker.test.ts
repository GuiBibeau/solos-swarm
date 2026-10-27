import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import test from "node:test";
import { normalizeContext } from "@earendil-works/pi-ai";
import type { ExtensionAPI, ProviderConfig } from "@earendil-works/pi-coding-agent";
import swarmBroker from "../extensions/broker.ts";

const names = ["claim_task", "board_read", "board_post", "submit_finding", "read_findings", "submit_artifact", "fetch_url", "run_code"];

test("real pinned Pi talks only to Unix broker and completes a tool loop", { timeout: 30_000 }, async (t) => {
  const dir = await mkdtemp(`${tmpdir()}/swarm-pi-`);
  const socket = `${dir}/broker.sock`;
  let modelCalls = 0;
  const toolCalls: string[] = [];
  // Longer than the Board limit on purpose: the boundary must shorten it, not
  // reject the call, or a model can loop on the validation error until its
  // budget is gone (math-swarm-20261025-02, agent-07).
  const boardMessage = `Checking A B ${"x".repeat(2100)}`;
  const server = createServer(async (req, res) => {
    try {
      assert.equal(req.headers.authorization, "Bearer session-test-token");
      const chunks: Buffer[] = [];
      for await (const chunk of req) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString());
      res.setHeader("Content-Type", "application/json");
      if (req.url === "/model") {
        assert.equal(body.stream, false);
        assert.equal(body.model, "deepseek-flash");
        assert.deepEqual(body.tools.map((tool: { function: { name: string } }) => tool.function.name).sort(), [...names].sort());
        assert.ok(body.messages.some((message: { role: string }) => message.role === "system"));
        const calls = [
          { id: "call-claim", type: "function", function: { name: "claim_task", arguments: "{}" } },
          { id: "call-board", type: "function", function: { name: "board_post", arguments: JSON.stringify({ message: boardMessage }) } },
          { id: "call-submit", type: "function", function: { name: "submit_finding", arguments: JSON.stringify({ task_id: "task-0", result: { inconsistencies: [] } }) } },
        ];
        modelCalls++;
        if (modelCalls > 1) {
          assert.equal(body.messages.filter((message: { role: string }) => message.role === "tool").length, 3);
          const thinking = body.messages.find((message: { role: string }) => message.role === "assistant");
          assert.equal(thinking.tool_calls.length, 3);
          const toolText = body.messages
            .filter((message: { role: string }) => message.role === "tool")
            .map((message: { content: unknown }) => typeof message.content === "string" ? message.content : JSON.stringify(message.content))
            .join(" ");
          assert.ok(toolText.includes("Message shortened"), "the model must be told the Board message was shortened");
        }
        res.end(JSON.stringify({
          id: `completion-${modelCalls}`, created: 1, model: "deepseek-flash", swarm_cost_microusd: 30,
          choices: [{ index: 0, message: modelCalls === 1 ? { role: "assistant", content: null, tool_calls: calls } : { role: "assistant", content: "Finished Ω\u2028 safely." }, finish_reason: modelCalls === 1 ? "tool_calls" : "stop" }],
          usage: { prompt_tokens: 20, completion_tokens: 20, total_tokens: 40, prompt_cache_hit_tokens: 5 },
        }));
      } else {
        const name = req.url?.split("/").at(-1) ?? "";
        assert.ok(names.includes(name));
        toolCalls.push(name);
        if (name === "claim_task") assert.deepEqual(body, {});
        if (name === "board_post") {
          assert.equal(body.message.length, 2048, "the boundary must shorten to the Board limit");
          assert.ok(body.message.startsWith("Checking A\u2028B "));
        }
        if (name === "submit_finding") assert.deepEqual(body, { task_id: "task-0", result: { inconsistencies: [] } });
        res.end(JSON.stringify({ task_id: "task-0", accepted: true }));
      }
    } catch (error) {
      res.statusCode = 500;
      res.end(JSON.stringify({ error: String(error) }));
    }
  });
  await new Promise<void>((done) => server.listen(socket, done));
  const child = spawn(process.execPath, [
    resolve("node_modules/@earendil-works/pi-coding-agent/dist/cli.js"),
    "--offline", "--mode", "rpc", "--provider", "swarm", "--model", "deepseek-flash", "--thinking", "off",
    "--no-session", "--no-builtin-tools", "--no-extensions", "--no-mcp", "--no-skills", "--no-prompt-templates",
    "--no-themes", "--no-context-files", "--no-approve", "--extension", resolve("extensions/broker.ts"),
  ], { cwd: dir, signal: t.signal, env: { PATH: process.env.PATH, HOME: dir, PI_CODING_AGENT_DIR: `${dir}/config`, SWARM_SOCKET: socket, SWARM_TOKEN: "session-test-token", SWARM_ROLE: "coordinator", PI_TELEMETRY: "0" }, stdio: ["pipe", "pipe", "pipe"] });
  let stderr = "";
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => { stderr += chunk; });
  const records: any[] = [];
  const settled = new Promise<void>((done, reject) => {
    let pending = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      pending += chunk;
      let newline;
      while ((newline = pending.indexOf("\n")) >= 0) {
        const line = pending.slice(0, newline);
        pending = pending.slice(newline + 1);
        if (!line) continue;
        try {
          const record = JSON.parse(line);
          records.push(record);
          if (record.type === "response" && record.success === false) reject(new Error(JSON.stringify(record)));
          if (record.type === "agent_settled") done();
        } catch (error) { reject(error); }
      }
    });
    child.on("error", reject);
    child.on("exit", (code) => reject(new Error(`Pi exited ${code}: ${stderr}`)));
  });
  try {
    child.stdin.write(`${JSON.stringify({ id: "prompt-1", type: "prompt", message: "Check your assigned records." })}\n`);
    await settled;
    assert.equal(modelCalls, 2, `${stderr}\n${JSON.stringify(records.filter((record) => record.type === "message_end"))}`);
    assert.deepEqual(toolCalls.sort(), ["board_post", "claim_task", "submit_finding"]);
    const messages = records.filter((record) => record.type === "message_end" && record.message.role === "assistant");
    assert.equal(messages.at(-1).message.stopReason, "stop");
    assert.equal(messages.at(-1).message.content[0].text, "Finished Ω\u2028 safely.");
    assert.equal(messages.at(-1).message.usage.cost.total, 0.00003);
    assert.equal(messages.at(-1).message.usage.cacheRead, 5);
    assert.ok(Math.abs(messages.at(-1).message.usage.cost.cacheRead - 0.00000003) < 1e-14);
  } finally {
    child.kill("SIGKILL");
    await new Promise<void>((done) => server.close(() => done()));
    await rm(dir, { recursive: true, force: true });
  }
});

test("worker tools exclude coordinator grants, including forged tool calls", () => {
  const previousRole = process.env.SWARM_ROLE;
  process.env.SWARM_ROLE = "worker";
  const declared: string[] = [];
  let guard: ((event: { toolName: string }) => unknown) | undefined;
  try {
    swarmBroker({
      registerProvider() {}, registerTool(tool) { declared.push(tool.name); },
      on(event, handler) { if (event === "tool_call") guard = handler; },
    } as ExtensionAPI);
    assert.deepEqual(declared.sort(), ["board_post", "board_read", "claim_task", "fetch_url", "run_code", "submit_finding"]);
    assert.equal((guard?.({ toolName: "submit_artifact" }) as { block: boolean }).block, true);
    assert.equal((guard?.({ toolName: "read_findings" }) as { block: boolean }).block, true);
    assert.equal(guard?.({ toolName: "submit_finding" }), undefined);
  } finally {
    if (previousRole === undefined) delete process.env.SWARM_ROLE; else process.env.SWARM_ROLE = previousRole;
  }
});

test("SWARM_GRANTS narrows the registered tools, so a Goal without run_code never advertises it", () => {
  const previousRole = process.env.SWARM_ROLE;
  const previousGrants = process.env.SWARM_GRANTS;
  process.env.SWARM_ROLE = "worker";
  process.env.SWARM_GRANTS = "claim_task,board_read,board_post,submit_finding,fetch_url";
  const declared: string[] = [];
  try {
    swarmBroker({
      registerProvider() {}, registerTool(tool) { declared.push(tool.name); }, on() {},
    } as ExtensionAPI);
    assert.deepEqual(declared.sort(), ["board_post", "board_read", "claim_task", "fetch_url", "submit_finding"]);
  } finally {
    if (previousRole === undefined) delete process.env.SWARM_ROLE; else process.env.SWARM_ROLE = previousRole;
    if (previousGrants === undefined) delete process.env.SWARM_GRANTS; else process.env.SWARM_GRANTS = previousGrants;
  }
});

test("provider rejects malformed responses and handles denial and cancellation without retrying", { timeout: 10_000 }, async (t) => {
  const dir = await mkdtemp(`${tmpdir()}/swarm-errors-`);
  const previousSocket = process.env.SWARM_SOCKET;
  const previousToken = process.env.SWARM_TOKEN;
  process.env.SWARM_SOCKET = `${dir}/broker.sock`;
  process.env.SWARM_TOKEN = "session-test-token";
  let behavior = "denied";
  let calls = 0;
  let requestStarted: () => void = () => {};
  const server = createServer((req, res) => {
    calls++;
    req.resume();
    requestStarted();
    if (behavior === "cancel") return;
    res.setHeader("Content-Type", "application/json");
    if (behavior === "denied") {
      res.statusCode = 403;
      res.end(JSON.stringify({ error: { message: "Denied by Goal budget" } }));
    } else res.end(JSON.stringify({ choices: [{ message: {}, finish_reason: "invalid" }] }));
  });
  await new Promise<void>((done) => server.listen(process.env.SWARM_SOCKET, done));
  let provider: ProviderConfig | undefined;
  swarmBroker({ registerProvider: (_name, config) => { provider = config; }, registerTool() {}, on() {} } as ExtensionAPI);
  assert.ok(provider?.streamSimple && provider.models?.[0]);
  const model = { ...provider.models[0], type: "chat" as const, api: "swarm-broker", provider: "swarm", baseUrl: "http://swarm.invalid/v1", input: ["text"] as ["text"] };
  const context = normalizeContext({ messages: [{ role: "user", content: "Hello", timestamp: 1 }] });
  try {
    const denied = await provider.streamSimple(model, context, { apiKey: "session-test-token", signal: t.signal, maxRetries: 0 }).result();
    assert.equal(denied.stopReason, "error");
    assert.match(denied.errorMessage ?? "", /Denied by Goal budget/);
    behavior = "malformed";
    const malformed = await provider.streamSimple(model, context, { apiKey: "session-test-token", signal: t.signal, maxRetries: 0 }).result();
    assert.equal(malformed.stopReason, "error");
    assert.match(malformed.errorMessage ?? "", /Invalid broker model response/);
    behavior = "cancel";
    const started = new Promise<void>((done) => { requestStarted = done; });
    const controller = new AbortController();
    const cancelled = provider.streamSimple(model, context, { apiKey: "session-test-token", signal: controller.signal, maxRetries: 0 }).result();
    await started;
    controller.abort();
    assert.equal((await cancelled).stopReason, "aborted");
    assert.equal(calls, 3);
  } finally {
    server.closeAllConnections();
    await new Promise<void>((done) => server.close(() => done()));
    if (previousSocket === undefined) delete process.env.SWARM_SOCKET; else process.env.SWARM_SOCKET = previousSocket;
    if (previousToken === undefined) delete process.env.SWARM_TOKEN; else process.env.SWARM_TOKEN = previousToken;
    await rm(dir, { recursive: true, force: true });
  }
});
