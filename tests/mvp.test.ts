import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { createPublicKey, verify } from "node:crypto";
import { access, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:https";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { broker } from "../extensions/mvp.ts";

test("MVP extension binds mTLS signatures and preserves unresolved intents across confirmation and lost acknowledgements", {timeout: 20_000}, async (t) => {
  const dir = await mkdtemp(join(tmpdir(), "solos-mvp-"));
  t.after(() => rm(dir, {recursive: true, force: true}));
  const openssl = (...args: string[]) => execFileSync("openssl", args, {cwd: dir, stdio: "ignore"});
  openssl("req", "-x509", "-newkey", "ed25519", "-nodes", "-subj", "/CN=Fixture CA", "-days", "1", "-keyout", "ca.key", "-out", "ca.pem");
  await writeFile(join(dir, "extensions.cnf"), "basicConstraints=CA:FALSE\nsubjectAltName=DNS:broker.test,URI:spiffe://fixture.test/tenant/t/goal/g/node/n/agent/a/incarnation/1\nextendedKeyUsage=serverAuth,clientAuth\n");
  for (const name of ["server", "client", "rotated"]) {
    openssl("req", "-new", "-newkey", "ed25519", "-nodes", "-subj", `/CN=${name}`, "-keyout", `${name}.key`, "-out", `${name}.csr`);
    openssl("x509", "-req", "-in", `${name}.csr`, "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial", "-days", "1", "-extfile", "extensions.cnf", "-out", `${name}.pem`);
  }
  const ca = await readFile(join(dir, "ca.pem"), "utf8");
  const identity = async (name: string) => writeFile(join(dir, "identity.json"), JSON.stringify({ca_pem: ca, certificate_pem: await readFile(join(dir, `${name}.pem`), "utf8"), private_key_pem: await readFile(join(dir, `${name}.key`), "utf8")}), {mode: 0o600});
  await identity("client");
  const ids = new Map<string, string[]>(); let actualEffects = 0;
  const applied = new Set<string>();
  const server = createServer({key: await readFile(join(dir, "server.key")), cert: await readFile(join(dir, "server.pem")), ca, requestCert: true, rejectUnauthorized: true, minVersion: "TLSv1.3", maxVersion: "TLSv1.3"}, async (req, res) => {
    try {
      const chunks: Buffer[] = []; for await (const chunk of req) chunks.push(chunk);
      const message = JSON.parse(Buffer.concat(chunks).toString());
      assert.equal(message.path, req.url);
      assert.equal(req.socket.authorized, true);
      const certificate = req.socket.getPeerCertificate();
      const pem = `-----BEGIN CERTIFICATE-----\n${certificate.raw.toString("base64").match(/.{1,64}/g)!.join("\n")}\n-----END CERTIFICATE-----\n`;
      assert.ok(verify(null, Buffer.from(`solos-swarm-request-v1\n${message.request_id}\n${message.issued_ms}\n${message.path}\n${message.payload}`), createPublicKey(pem), Buffer.from(message.signature, "base64")));
      const history = ids.get(message.path) ?? []; history.push(message.request_id); ids.set(message.path, history);
      res.setHeader("Content-Type", "application/json");
      if ((message.path === "/tool/confirm" || message.path === "/tool/rotate") && history.length === 1) return res.end(JSON.stringify({status: "confirmation_required", intent: message.request_id}));
      if (message.path === "/tool/lost" && !applied.has(message.request_id)) {applied.add(message.request_id); actualEffects++; req.socket.destroy(); return;}
      res.end(JSON.stringify({accepted: true}));
    } catch (error) {res.statusCode = 500; res.end(JSON.stringify({error: String(error)}));}
  });
  const socket = join(dir, "broker.sock");
  await new Promise<void>((resolve) => server.listen(socket, resolve));
  t.after(() => new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve())));
  const old = [process.env.SWARM_SOCKET, process.env.SWARM_IDENTITY_DIR, process.env.SWARM_BROKER_NAME];
  process.env.SWARM_SOCKET = socket; process.env.SWARM_IDENTITY_DIR = dir; process.env.SWARM_BROKER_NAME = "broker.test";
  t.after(() => {for (const [index, name] of ["SWARM_SOCKET", "SWARM_IDENTITY_DIR", "SWARM_BROKER_NAME"].entries()) {if (old[index] === undefined) delete process.env[name]; else process.env[name] = old[index];}});
  assert.equal((await (await broker("/tool/confirm", {x: 1}, undefined, "original")).json()).status, "confirmation_required");
  assert.ok((await broker("/tool/confirm", {x: 1}, undefined, "new-pi-id")).ok);
  assert.deepEqual(ids.get("/tool/confirm"), ["original", "original"]);
  await assert.rejects(broker("/tool/lost", {x: 2}, undefined, "lost"));
  assert.ok((await broker("/tool/lost", {x: 2}, undefined, "replacement")).ok);
  assert.deepEqual(ids.get("/tool/lost"), ["lost", "lost"]); assert.equal(actualEffects, 1);
  await broker("/tool/rotate", {x: 3}, undefined, "pending-rotation");
  await identity("rotated");
  await assert.rejects(broker("/tool/rotate", {x: 3}), /Identity rotated/);
  assert.deepEqual(ids.get("/tool/rotate"), ["pending-rotation"]);
  await identity("client"); await broker("/tool/rotate", {x: 3});
  assert.deepEqual(ids.get("/tool/rotate"), ["pending-rotation", "pending-rotation"]);
  assert.ok((await broker("/tool/confirm", {x: 1}, undefined, "fresh-intent")).ok);
  assert.deepEqual(ids.get("/tool/confirm"), ["original", "original", "fresh-intent"]);
});

test("pinned Pi preserves iterative attempts and executes the final scoped batch sequentially without another model call", {timeout: 30_000}, async (t) => {
  assert.equal(JSON.parse(await readFile(resolve("node_modules/@earendil-works/pi-coding-agent/package.json"), "utf8")).version, "1.1.0");
  const dir = await mkdtemp(join(tmpdir(), "solos-mvp-pi-"));
  const openssl = (...args: string[]) => execFileSync("openssl", args, {cwd: dir, stdio: "ignore"});
  openssl("req", "-x509", "-newkey", "ed25519", "-nodes", "-subj", "/CN=MVP Pi Fixture CA", "-days", "1", "-keyout", "ca.key", "-out", "ca.pem");
  for (const name of ["server", "worker"]) {
    const uri = `spiffe://fixture.test/tenant/t/goal/g/node/n/agent/${name}/incarnation/1`;
    await writeFile(join(dir, `${name}.cnf`), `basicConstraints=CA:FALSE\nsubjectAltName=DNS:broker.test,URI:${uri}\nextendedKeyUsage=serverAuth,clientAuth\n`);
    openssl("req", "-new", "-newkey", "ed25519", "-nodes", "-subj", `/CN=${name}`, "-keyout", `${name}.key`, "-out", `${name}.csr`);
    openssl("x509", "-req", "-in", `${name}.csr`, "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial", "-days", "1", "-extfile", `${name}.cnf`, "-out", `${name}.pem`);
  }
  const ca = await readFile(join(dir, "ca.pem"), "utf8");
  await writeFile(join(dir, "identity.json"), JSON.stringify({ca_pem: ca, certificate_pem: await readFile(join(dir, "worker.pem"), "utf8"), private_key_pem: await readFile(join(dir, "worker.key"), "utf8")}), {mode: 0o600});
  const modelId = "mvp-fixture-v1";
  const tools = [
    {name: "claim_task", description: "Claim the admitted Task", parameters: {type: "object", properties: {}, additionalProperties: false}},
    {name: "submit_finding", description: "Submit the assigned evidence", parameters: {type: "object", properties: {body: {type: "object", properties: {}, additionalProperties: true}}, required: ["body"], additionalProperties: false}},
    {name: "submit_attempt", description: "Submit a candidate for evaluation", parameters: {type: "object", properties: {payload: {type: "object", additionalProperties: true}}, required: ["payload"], additionalProperties: false}},
    {name: "complete_task", description: "Complete the assigned Task using its own Finding", parameters: {type: "object", properties: {result_ref: {type: "string"}}, required: ["result_ref"], additionalProperties: false}},
  ];
  const calls: Array<{path: string; id: string; payload: any}> = [];
  const serverFailures: Error[] = [];
  let modelCalls = 0;
  let claimed = false;
  let finalSubmitted = false;
  let completed = false;
  const server = createServer({key: await readFile(join(dir, "server.key")), cert: await readFile(join(dir, "server.pem")), ca, requestCert: true, rejectUnauthorized: true, minVersion: "TLSv1.3", maxVersion: "TLSv1.3"}, async (req, res) => {
    try {
      const chunks: Buffer[] = []; for await (const chunk of req) chunks.push(chunk);
      const message = JSON.parse(Buffer.concat(chunks).toString());
      assert.equal(req.socket.authorized, true);
      assert.equal(message.path, req.url);
      assert.ok(Math.abs(Date.now() - message.issued_ms) < 10_000);
      const certificate = req.socket.getPeerCertificate();
      assert.ok(certificate.subjectaltname?.includes("URI:spiffe://fixture.test/tenant/t/goal/g/node/n/agent/worker/incarnation/1"));
      const pem = `-----BEGIN CERTIFICATE-----\n${certificate.raw.toString("base64").match(/.{1,64}/g)!.join("\n")}\n-----END CERTIFICATE-----\n`;
      assert.ok(verify(null, Buffer.from(`solos-swarm-request-v1\n${message.request_id}\n${message.issued_ms}\n${message.path}\n${message.payload}`), createPublicKey(pem), Buffer.from(message.signature, "base64")));
      assert.equal(req.headers.authorization, undefined, "The broker is authenticated by the workload key, not a provider credential");
      const payload = JSON.parse(message.payload);
      calls.push({path: message.path, id: message.request_id, payload});
      res.setHeader("Content-Type", "application/json");
      if (message.path === "/session") {
        assert.deepEqual(payload, {});
        return res.end(JSON.stringify({goal_id: "g", task_id: "task-1", tools, model: {
          id: modelId, name: "Scoped fixture model", reasoning: false, input: ["text"], contextWindow: 32768, maxTokens: 1024,
          cost: {input: 0, output: 0, cacheRead: 0, cacheWrite: 0},
          compat: {supportsStore: false, supportsDeveloperRole: false, maxTokensField: "max_tokens", supportsStrictMode: false},
        }}));
      }
      if (message.path === "/model") {
        modelCalls++;
        assert.equal(payload.model, modelId);
        assert.equal(payload.stream, false);
        assert.deepEqual(payload.tools.map((tool: any) => tool.function.name).sort(), tools.map((tool) => tool.name).sort());
        assert.ok(payload.messages.some((message: any) => message.role === "system"));
        const claim = {id: "claim-1", type: "function", function: {name: "claim_task", arguments: "{}"}};
        const finding = {id: "finding-1", type: "function", function: {name: "submit_finding", arguments: JSON.stringify({body: {fact: "Verified Ω\u2028 from task-1"}})}};
        const forged = {id: "forged-1", type: "function", function: {name: "bash", arguments: JSON.stringify({command: "touch forbidden-side-effect"})}};
        const attempt = {id: "attempt-search", type: "function", function: {name: "submit_attempt", arguments: JSON.stringify({payload: {phase: "search"}})}};
        const finalAttempt = {id: "attempt-final", type: "function", function: {name: "submit_attempt", arguments: JSON.stringify({payload: {phase: "final"}})}};
        const complete = {id: "complete-final", type: "function", function: {name: "complete_task", arguments: JSON.stringify({result_ref: "finding-result-1"})}};
        if (modelCalls > 1) {
          assert.ok(payload.messages.some((message: any) => message.role === "tool" && message.tool_call_id === "claim-1" && message.content.includes("task-1")));
        }
        if (modelCalls > 2) {
          assert.ok(payload.messages.some((message: any) => message.role === "tool" && message.tool_call_id === "finding-1" && message.content.includes("accepted")));
          assert.ok(payload.messages.some((message: any) => message.role === "tool" && message.tool_call_id === "forged-1" && /not found|compiled definition/i.test(message.content)));
        }
        if (modelCalls === 4) assert.ok(payload.messages.some((message: any) => message.role === "tool" && message.tool_call_id === "attempt-search" && message.content.includes("accepted")));
        assert.ok(modelCalls <= 5, "The final batch must settle without a sixth model request");
        const selected = modelCalls === 1 ? [claim] : modelCalls === 2 ? [finding, forged] : modelCalls === 3 ? [attempt] : modelCalls === 5 ? [finalAttempt, complete] : null;
        return res.end(JSON.stringify({id: `mvp-receipt-${modelCalls}`, created: 1, model: modelId, swarm_cost_microusd: 7,
          choices: [{index: 0, message: selected ? {role: "assistant", content: null, tool_calls: selected} : {role: "assistant", content: "MVP finished Ω\u2028 with scoped evidence."}, finish_reason: selected ? "tool_calls" : "stop"}],
          usage: {prompt_tokens: 20, completion_tokens: 3, total_tokens: 23, prompt_cache_hit_tokens: 2},
        }));
      }
      if (message.path === "/tool/claim_task") {
        assert.equal(claimed, false);
        assert.deepEqual(payload, {});
        assert.equal(message.request_id, "tool-claim-1");
        claimed = true;
        return res.end(JSON.stringify({task_id: "task-1", input: {fact: "synthetic scoped input"}}));
      }
      if (message.path === "/tool/submit_attempt") {
        assert.equal(claimed, true);
        if (message.request_id === "tool-attempt-final") {
          assert.deepEqual(payload, {payload: {phase: "final"}});
          // Pi defaults to parallel tools. The extension must wait for this reply
          // before executing CompleteTask from the same assistant message.
          await new Promise((done) => setTimeout(done, 100));
          finalSubmitted = true;
        } else {
          assert.equal(message.request_id, "tool-attempt-search");
          assert.deepEqual(payload, {payload: {phase: "search"}});
        }
        return res.end(JSON.stringify({accepted: true}));
      }
      if (message.path === "/tool/complete_task") {
        assert.equal(finalSubmitted, true, "CompleteTask must follow the committed final submission");
        assert.deepEqual(payload, {result_ref: "finding-result-1"});
        assert.equal(message.request_id, "tool-complete-final");
        completed = true;
        return res.end(JSON.stringify({accepted: true}));
      }
      assert.equal(message.path, "/tool/submit_finding");
      assert.equal(claimed, true);
      assert.equal(message.request_id, "tool-finding-1");
      assert.deepEqual(payload, {body: {fact: "Verified Ω\u2028 from task-1"}});
      res.end(JSON.stringify({accepted: true, id: "finding-result-1"}));
    } catch (error) {
      serverFailures.push(error instanceof Error ? error : new Error(String(error)));
      res.statusCode = 500;
      res.end(JSON.stringify({error: String(error)}));
    }
  });
  const socket = join(dir, "broker.sock");
  await new Promise<void>((done) => server.listen(socket, done));
  const child = spawn(process.execPath, [
    resolve("node_modules/@earendil-works/pi-coding-agent/dist/cli.js"),
    "--offline", "--mode", "rpc", "--provider", "swarm", "--model", modelId, "--thinking", "off",
    "--no-session", "--no-builtin-tools", "--no-extensions", "--no-mcp", "--no-skills", "--no-prompt-templates",
    "--no-themes", "--no-context-files", "--no-approve", "--extension", resolve("extensions/mvp.ts"),
  ], {cwd: dir, signal: t.signal, env: {PATH: process.env.PATH, HOME: dir, PI_CODING_AGENT_DIR: join(dir, "config"), SWARM_SOCKET: socket, SWARM_IDENTITY_DIR: dir, SWARM_BROKER_NAME: "broker.test", PI_TELEMETRY: "0"}, stdio: ["pipe", "pipe", "pipe"]});
  let stderr = "";
  child.stderr.setEncoding("utf8"); child.stderr.on("data", (chunk) => {stderr += chunk;});
  const records: any[] = [];
  let pending: {done: () => void; reject: (error: unknown) => void} | undefined;
  const waitForSettled = () => new Promise<void>((done, reject) => {pending = {done, reject};});
  {
    let buffer = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      buffer += chunk;
      let newline: number;
      while ((newline = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, newline); buffer = buffer.slice(newline + 1);
        if (!line) continue;
        try {
          const record = JSON.parse(line); records.push(record);
          if (record.type === "response" && record.success === false) pending?.reject(new Error(JSON.stringify(record)));
          if (record.type === "extension_error") pending?.reject(new Error(JSON.stringify(record)));
          if (record.type === "agent_settled") {pending?.done(); pending = undefined;}
        } catch (error) {pending?.reject(error);}
      }
    });
    child.on("error", (error) => pending?.reject(error));
    child.on("exit", (code) => pending?.reject(new Error(`MVP Pi exited ${code}: ${stderr}`)));
  }
  try {
    const settled = waitForSettled();
    child.stdin.write(`${JSON.stringify({id: "mvp-prompt", type: "prompt", message: "Claim your admitted task, submit its evidence, then finish."})}\n`);
    await settled;
    assert.deepEqual(serverFailures, [], stderr);
    assert.equal(modelCalls, 4, `${stderr}\n${JSON.stringify(records.filter((record) => record.type === "message_end"))}`);
    assert.equal(calls[0].path, "/session");
    assert.deepEqual(calls.filter((call) => call.path.startsWith("/tool/")).map((call) => call.path), ["/tool/claim_task", "/tool/submit_finding", "/tool/submit_attempt"]);
    assert.equal(new Set(calls.map((call) => call.id)).size, calls.length);
    await assert.rejects(access(join(dir, "forbidden-side-effect")));
    const final = records.filter((record) => record.type === "message_end" && record.message.role === "assistant").at(-1)?.message;
    assert.equal(final?.stopReason, "stop");
    assert.equal(final?.content[0].text, "MVP finished Ω\u2028 with scoped evidence.");
    assert.equal(final?.usage.cost.total, 0.000007);
    assert.equal(final?.usage.cacheRead, 2);
    const finalSettled = waitForSettled();
    child.stdin.write(`${JSON.stringify({id: "mvp-final", type: "prompt", message: "Submit the final candidate and complete the Task in one ordered assistant batch."})}\n`);
    await finalSettled;
    assert.deepEqual(serverFailures, [], stderr);
    assert.equal(modelCalls, 5, "The final two-tool batch must not request another model completion");
    assert.equal(finalSubmitted, true);
    assert.equal(completed, true);
    assert.deepEqual(calls.filter((call) => call.path.startsWith("/tool/")).slice(-2).map((call) => call.path), ["/tool/submit_attempt", "/tool/complete_task"]);
  } finally {
    child.kill("SIGKILL");
    server.closeAllConnections();
    await new Promise<void>((done) => server.close(() => done()));
    await rm(dir, {recursive: true, force: true});
  }
});
