import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
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
  let pausedReplies = 2;
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
      if (req.url === "/model" && pausedReplies > 0) {
        pausedReplies--; // a paused broker must be waited out, not treated as an error or a spent call
        res.statusCode = 503;
        res.end(JSON.stringify({ paused: true, retry_after_s: 0.2 }));
        return;
      }
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

const routerNames = ["router_decide", "router_read"];

test("the router tool set talks to the broker, takes its system prompt and sees a refused decision as a tool error", { timeout: 30_000 }, async (t) => {
  const dir = await mkdtemp(`${tmpdir()}/swarm-router-`);
  const socket = `${dir}/broker.sock`;
  let modelCalls = 0;
  let sessionCalls = 0;
  const toolCalls: string[] = [];
  const refusal = "refused: ev_1 is not running";
  const server = createServer(async (req, res) => {
    try {
      assert.equal(req.headers.authorization, "Bearer router-test-token");
      const chunks: Buffer[] = [];
      for await (const chunk of req) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString());
      res.setHeader("Content-Type", "application/json");
      if (req.url === "/session") {
        sessionCalls++;
        res.end(JSON.stringify({ system_prompt: "You are the test Router. Decide every candidate." }));
      } else if (req.url === "/model") {
        modelCalls++;
        assert.deepEqual(body.tools.map((tool: { function: { name: string } }) => tool.function.name).sort(), routerNames);
        const decide = body.tools.find((tool: { function: { name: string } }) => tool.function.name === "router_decide");
        assert.deepEqual(decide.function.parameters.properties.decision.enum, ["accept", "ignore", "stop"]);
        const system = body.messages.find((message: { role: string }) => message.role === "system");
        assert.ok(JSON.stringify(system?.content ?? "").includes("You are the test Router."), JSON.stringify(body.messages));
        if (modelCalls === 1) {
          const calls = [
            { id: "call-read", type: "function", function: { name: "router_read", arguments: JSON.stringify({ id: "t1" }) } },
            { id: "call-decide", type: "function", function: { name: "router_decide", arguments: JSON.stringify({ eval: "ev_1", decision: "ignore", reason: "chatter" }) } },
          ];
          res.end(JSON.stringify({
            id: "completion-1", created: 1, model: "deepseek-flash", swarm_cost_microusd: 30,
            choices: [{ index: 0, message: { role: "assistant", content: null, tool_calls: calls }, finish_reason: "tool_calls" }],
            usage: { prompt_tokens: 20, completion_tokens: 20, total_tokens: 40 },
          }));
        } else {
          const results = body.messages.filter((message: { role: string }) => message.role === "tool")
            .map((message: { content: unknown }) => typeof message.content === "string" ? message.content : JSON.stringify(message.content));
          assert.equal(results.length, 2);
          assert.ok(results.some((text: string) => text.includes(refusal)), `the model must see the Engine's refusal: ${results}`);
          assert.ok(results.some((text: string) => text.includes("\"kind\":\"trigger\"")));
          res.end(JSON.stringify({
            id: "completion-2", created: 1, model: "deepseek-flash", swarm_cost_microusd: 30,
            choices: [{ index: 0, message: { role: "assistant", content: "Decided." }, finish_reason: "stop" }],
            usage: { prompt_tokens: 20, completion_tokens: 5, total_tokens: 25 },
          }));
        }
      } else {
        const name = req.url?.split("/").at(-1) ?? "";
        toolCalls.push(name);
        if (name === "router_read") {
          assert.deepEqual(body, { id: "t1" });
          res.end(JSON.stringify({ kind: "trigger", id: "t1", threads: ["T1"], tickets: [] }));
        } else {
          assert.equal(name, "router_decide");
          assert.deepEqual(body, { eval: "ev_1", decision: "ignore", reason: "chatter" });
          res.statusCode = 409;
          res.end(JSON.stringify({ error: refusal }));
        }
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
  ], { cwd: dir, signal: t.signal, env: { PATH: process.env.PATH, HOME: dir, PI_CODING_AGENT_DIR: `${dir}/config`, SWARM_SOCKET: socket, SWARM_TOKEN: "router-test-token", SWARM_ROLE: "router", SWARM_GRANTS: "router_decide,router_read", SWARM_TOOLSET: "router", PI_TELEMETRY: "0" }, stdio: ["pipe", "pipe", "pipe"] });
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
          if (record.type === "extension_error") reject(new Error(JSON.stringify(record)));
          if (record.type === "agent_settled") done();
        } catch (error) { reject(error); }
      }
    });
    child.on("error", reject);
    child.on("exit", (code) => reject(new Error(`Pi exited ${code}: ${stderr}`)));
  });
  try {
    child.stdin.write(`${JSON.stringify({ id: "prompt-1", type: "prompt", message: "eval ev_1 thread T1 strength explicit" })}\n`);
    await settled;
    assert.equal(modelCalls, 2, `${stderr}\n${JSON.stringify(records.filter((record) => record.type === "message_end"))}`);
    assert.equal(sessionCalls, 1);
    assert.deepEqual(toolCalls.sort(), routerNames);
    const ends = records.filter((record) => record.type === "tool_execution_end");
    const decided = ends.find((record) => record.toolName === "router_decide");
    assert.equal(decided?.isError, true, JSON.stringify(ends));
  } finally {
    child.kill("SIGKILL");
    await new Promise<void>((done) => server.close(() => done()));
    await rm(dir, { recursive: true, force: true });
  }
});

test("SWARM_TOOLSET=router registers only the Router tools, whatever the role and grants say", () => {
  const saved = { role: process.env.SWARM_ROLE, grants: process.env.SWARM_GRANTS, toolset: process.env.SWARM_TOOLSET };
  process.env.SWARM_ROLE = "coordinator";
  process.env.SWARM_GRANTS = names.join(",");
  process.env.SWARM_TOOLSET = "router";
  const declared: string[] = [];
  const events: string[] = [];
  let guard: ((event: { toolName: string }) => unknown) | undefined;
  const restore = (name: string, value: string | undefined) => { if (value === undefined) delete process.env[name]; else process.env[name] = value; };
  try {
    swarmBroker({
      registerProvider() {}, registerTool(tool) { declared.push(tool.name); },
      on(event, handler) { events.push(event); if (event === "tool_call") guard = handler; },
    } as ExtensionAPI);
    assert.deepEqual(declared.sort(), routerNames);
    assert.ok(events.includes("before_agent_start"), "the Router takes its system prompt from the broker");
    for (const forged of ["run_code", "fetch_url", "claim_task", "submit_artifact"]) {
      assert.equal((guard?.({ toolName: forged }) as { block: boolean }).block, true, forged);
    }
    assert.equal(guard?.({ toolName: "router_decide" }), undefined);
    process.env.SWARM_TOOLSET = "admin";
    assert.throws(() => swarmBroker({ registerProvider() {}, registerTool() {}, on() {} } as ExtensionAPI), /Unknown SWARM_TOOLSET/);
    delete process.env.SWARM_TOOLSET;
    const swarm: string[] = [];
    const swarmEvents: string[] = [];
    swarmBroker({ registerProvider() {}, registerTool(tool) { swarm.push(tool.name); }, on(event) { swarmEvents.push(event); } } as ExtensionAPI);
    assert.deepEqual(swarm.sort(), [...names].sort(), "unset keeps the swarm tool set");
    assert.ok(!swarmEvents.includes("before_agent_start"), "a swarm agent keeps Pi's own system prompt");
  } finally {
    restore("SWARM_ROLE", saved.role);
    restore("SWARM_GRANTS", saved.grants);
    restore("SWARM_TOOLSET", saved.toolset);
  }
});

const workerNames = ["fetch_url", "run_code", "worker_handback", "worker_report"];

test("SWARM_TOOLSET=worker registers the worker tools that SWARM_GRANTS names and blocks every other call", () => {
  const saved = { role: process.env.SWARM_ROLE, grants: process.env.SWARM_GRANTS, toolset: process.env.SWARM_TOOLSET };
  const restore = (name: string, value: string | undefined) => { if (value === undefined) delete process.env[name]; else process.env[name] = value; };
  const load = () => {
    const declared: string[] = [];
    const events: string[] = [];
    let guard: ((event: { toolName: string }) => unknown) | undefined;
    swarmBroker({
      registerProvider() {}, registerTool(tool) { declared.push(tool.name); },
      on(event, handler) { events.push(event); if (event === "tool_call") guard = handler; },
    } as ExtensionAPI);
    return { declared: declared.sort(), events, guard: (name: string) => guard?.({ toolName: name }) as { block: boolean } | undefined };
  };
  try {
    process.env.SWARM_ROLE = "ticket-worker";
    process.env.SWARM_TOOLSET = "worker";
    delete process.env.SWARM_GRANTS;
    assert.deepEqual(load().declared, workerNames);
    process.env.SWARM_GRANTS = "worker_report,worker_handback,fetch_url";
    const narrowed = load();
    assert.deepEqual(narrowed.declared, ["fetch_url", "worker_handback", "worker_report"], "a host without run_code never advertises it");
    assert.ok(narrowed.events.includes("before_agent_start"), "a worker takes its system prompt from the broker");
    for (const forged of ["run_code", "router_decide", "claim_task", "submit_finding", "board_post"]) {
      assert.equal(narrowed.guard(forged)?.block, true, forged);
    }
    assert.equal(narrowed.guard("worker_report"), undefined);
    process.env.SWARM_ROLE = "coordinator";
    process.env.SWARM_GRANTS = names.join(",");
    assert.deepEqual(load().declared, ["fetch_url", "run_code"], "swarm grants and the coordinator role add no swarm tool");
  } finally {
    restore("SWARM_ROLE", saved.role);
    restore("SWARM_GRANTS", saved.grants);
    restore("SWARM_TOOLSET", saved.toolset);
  }
});

test("the worker tool set runs code, sees a refused report as a tool error, and takes its system prompt", { timeout: 30_000 }, async (t) => {
  const dir = await mkdtemp(`${tmpdir()}/swarm-worker-`);
  const socket = `${dir}/broker.sock`;
  let modelCalls = 0;
  let sessionCalls = 0;
  const toolCalls: string[] = [];
  const refusal = "refused: tk_1 has ended";
  const server = createServer(async (req, res) => {
    try {
      assert.equal(req.headers.authorization, "Bearer worker-test-token");
      const chunks: Buffer[] = [];
      for await (const chunk of req) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString());
      res.setHeader("Content-Type", "application/json");
      if (req.url === "/session") {
        sessionCalls++;
        res.end(JSON.stringify({ system_prompt: "You are the test worker. Report with worker_report." }));
      } else if (req.url === "/model") {
        modelCalls++;
        assert.deepEqual(body.tools.map((tool: { function: { name: string } }) => tool.function.name).sort(), workerNames);
        const report = body.tools.find((tool: { function: { name: string } }) => tool.function.name === "worker_report");
        assert.deepEqual(report.function.parameters.properties.status.enum, ["progress", "waiting", "ready", "blocked", "failed", "dropped"]);
        const system = body.messages.find((message: { role: string }) => message.role === "system");
        assert.ok(JSON.stringify(system?.content ?? "").includes("You are the test worker."), JSON.stringify(body.messages));
        if (modelCalls === 1) {
          const calls = [
            { id: "call-code", type: "function", function: { name: "run_code", arguments: JSON.stringify({ code: "echo ok" }) } },
            { id: "call-report", type: "function", function: { name: "worker_report", arguments: JSON.stringify({ status: "ready", text: "Done: ok" }) } },
          ];
          res.end(JSON.stringify({
            id: "completion-1", created: 1, model: "deepseek-flash", swarm_cost_microusd: 30,
            choices: [{ index: 0, message: { role: "assistant", content: null, tool_calls: calls }, finish_reason: "tool_calls" }],
            usage: { prompt_tokens: 20, completion_tokens: 20, total_tokens: 40 },
          }));
        } else {
          const results = body.messages.filter((message: { role: string }) => message.role === "tool")
            .map((message: { content: unknown }) => typeof message.content === "string" ? message.content : JSON.stringify(message.content));
          assert.equal(results.length, 2);
          assert.ok(results.some((text: string) => text.includes(refusal)), `the model must see the Engine's refusal: ${results}`);
          assert.ok(results.some((text: string) => text.includes("\"exit_code\":0")));
          res.end(JSON.stringify({
            id: "completion-2", created: 1, model: "deepseek-flash", swarm_cost_microusd: 30,
            choices: [{ index: 0, message: { role: "assistant", content: "Reported." }, finish_reason: "stop" }],
            usage: { prompt_tokens: 20, completion_tokens: 5, total_tokens: 25 },
          }));
        }
      } else {
        const name = req.url?.split("/").at(-1) ?? "";
        toolCalls.push(name);
        if (name === "run_code") {
          assert.deepEqual(body, { code: "echo ok" });
          res.end(JSON.stringify({ exit_code: 0, stdout: "ok\n", stderr: "", timed_out: false, duration_ms: 3, truncated: false }));
        } else {
          assert.equal(name, "worker_report");
          assert.deepEqual(body, { status: "ready", text: "Done: ok" });
          res.statusCode = 409;
          res.end(JSON.stringify({ error: refusal }));
        }
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
  ], { cwd: dir, signal: t.signal, env: { PATH: process.env.PATH, HOME: dir, PI_CODING_AGENT_DIR: `${dir}/config`, SWARM_SOCKET: socket, SWARM_TOKEN: "worker-test-token", SWARM_ROLE: "ticket-worker", SWARM_GRANTS: workerNames.join(","), SWARM_TOOLSET: "worker", PI_TELEMETRY: "0" }, stdio: ["pipe", "pipe", "pipe"] });
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
          if (record.type === "extension_error") reject(new Error(JSON.stringify(record)));
          if (record.type === "agent_settled") done();
        } catch (error) { reject(error); }
      }
    });
    child.on("error", reject);
    child.on("exit", (code) => reject(new Error(`Pi exited ${code}: ${stderr}`)));
  });
  try {
    child.stdin.write(`${JSON.stringify({ id: "prompt-1", type: "prompt", message: "Ticket tk_1: say ok." })}\n`);
    await settled;
    assert.equal(modelCalls, 2, `${stderr}\n${JSON.stringify(records.filter((record) => record.type === "message_end"))}`);
    assert.equal(sessionCalls, 1);
    assert.deepEqual(toolCalls.sort(), ["run_code", "worker_report"]);
    const ends = records.filter((record) => record.type === "tool_execution_end");
    assert.equal(ends.find((record) => record.toolName === "worker_report")?.isError, true, JSON.stringify(ends));
    assert.equal(ends.find((record) => record.toolName === "run_code")?.isError, false, JSON.stringify(ends));
  } finally {
    child.kill("SIGKILL");
    await new Promise<void>((done) => server.close(() => done()));
    await rm(dir, { recursive: true, force: true });
  }
});

test("the router and worker tool schemas match the Rust registry that GET /tools and solos-swarm mcp serve", () => {
  const saved = { grants: process.env.SWARM_GRANTS, toolset: process.env.SWARM_TOOLSET };
  const restore = (name: string, value: string | undefined) => { if (value === undefined) delete process.env[name]; else process.env[name] = value; };
  // The Rust test agent_tool_registry_matches_its_fixture holds the registry to the same file.
  const registry = JSON.parse(readFileSync(resolve("tests/fixtures/agent-tools.json"), "utf8"));
  try {
    delete process.env.SWARM_GRANTS;
    for (const toolset of ["router", "worker"]) {
      process.env.SWARM_TOOLSET = toolset;
      const declared: unknown[] = [];
      swarmBroker({
        registerProvider() {}, registerTool(tool) { declared.push({ name: tool.name, description: tool.description, input_schema: tool.parameters }); }, on() {},
      } as ExtensionAPI);
      assert.deepEqual(JSON.parse(JSON.stringify(declared)), registry[toolset], toolset);
    }
  } finally {
    restore("SWARM_GRANTS", saved.grants);
    restore("SWARM_TOOLSET", saved.toolset);
  }
});
