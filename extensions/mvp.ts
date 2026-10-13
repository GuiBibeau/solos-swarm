import { request } from "node:https";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { createHash, createPrivateKey, randomUUID, sign } from "node:crypto";
import { Type, lazyStream, openAICompletionsApi } from "@earendil-works/pi-ai/compat";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const pending = new Map<string, { id: string; certificate: string; active: boolean }>();

export async function broker(path: string, payload: unknown, signal?: AbortSignal, requestId = randomUUID()): Promise<Response> {
  const socketPath = process.env.SWARM_SOCKET;
  const identityDir = process.env.SWARM_IDENTITY_DIR;
  const serverName = process.env.SWARM_BROKER_NAME;
  if (!socketPath || !identityDir || !serverName) throw new Error("Workload broker configuration is missing");
  // Read the atomically rotated identity for every connection; never retain the old leaf key.
  const identity = JSON.parse(readFileSync(join(identityDir, "identity.json"), "utf8"));
  const key = createPrivateKey(identity.private_key_pem);
  const issued_ms = Date.now();
  const data = JSON.stringify(payload);
  if (Buffer.byteLength(data) > 1024 * 1024) throw new Error("Broker request exceeds 1 MiB");
  const binding = createHash("sha256").update(`${path}\n${data}`).digest("hex");
  const certificate = createHash("sha256").update(identity.certificate_pem).digest("hex");
  const previous = pending.get(binding);
  if (previous?.active) throw new Error(`Identical action ${previous.id} is already in flight; await its recorded outcome.`);
  if (previous && previous.certificate !== certificate) throw new Error(`Identity rotated while intent ${previous.id} remains unresolved; inspect it before creating another action.`);
  requestId = previous?.id ?? requestId;
  if (!previous && pending.size >= 64) throw new Error("Too many unresolved broker intents; operator reconciliation is required.");
  pending.set(binding, { id: requestId, certificate, active: true });
  const signature = sign(null, Buffer.from(`solos-swarm-request-v1\n${requestId}\n${issued_ms}\n${path}\n${data}`), key).toString("base64");
  const body = JSON.stringify({ request_id: requestId, issued_ms, path, payload: data, signature });
  return new Promise((resolve, reject) => {
    const failed = (error: Error) => { const entry = pending.get(binding); if (entry) entry.active = false; reject(error); };
    const req = request({ socketPath, servername: serverName, path, method: "POST", signal,
      rejectUnauthorized: true, ca: identity.ca_pem, cert: identity.certificate_pem, key: identity.private_key_pem,
      minVersion: "TLSv1.3", maxVersion: "TLSv1.3", agent: false,
      headers: { "Content-Type": "application/json", "Content-Length": Buffer.byteLength(body) },
    }, (res) => {
      const chunks: Buffer[] = []; let size = 0;
      res.on("data", (chunk: Buffer) => {
        size += chunk.length;
        if (size > 4 * 1024 * 1024) req.destroy(new Error("Broker response exceeds 4 MiB"));
        else chunks.push(chunk);
      });
      res.on("error", failed);
      res.on("end", () => {
        const bytes = Buffer.concat(chunks);
        let value: { status?: string } | undefined;
        try { value = JSON.parse(bytes.toString("utf8")); } catch { /* Partial/invalid outcomes remain unresolved. */ }
        if (res.statusCode === 200 && value && value.status !== "confirmation_required") pending.delete(binding);
        else { const entry = pending.get(binding); if (entry) entry.active = false; }
        resolve(new Response(bytes, { status: res.statusCode ?? 500, headers: { "Content-Type": "application/json" } }));
      });
    });
    req.setTimeout(120_000, () => req.destroy(new Error("Broker deadline reached; inspect the effect before retry")));
    req.on("error", failed); req.end(body);
  });
}

export default async function swarmMvp(pi: ExtensionAPI) {
  const response = await broker("/session", {});
  if (!response.ok) throw new Error(`Workload session denied: ${await response.text()}`);
  const session = await response.json();
  if (!session.model?.id || !Array.isArray(session.tools) || session.tools.length === 0) throw new Error("Missing compiled agent definition");
  pi.registerProvider("swarm", {
    baseUrl: "https://broker.invalid/v1", api: "swarm-broker", apiKey: "workload-identity",
    models: [session.model],
    streamSimple(model, context, options) {
      let costMicrousd: number | undefined;
      const upstream = openAICompletionsApi().streamSimple({ ...model, api: "openai-completions" }, context, {
        ...options, maxRetries: 0,
        fetch: async (input, init) => {
          const incoming = new Request(input, init);
          const payload = await incoming.json();
          const response = await broker("/model", { ...payload, stream: false }, incoming.signal);
          if (!response.ok) return response;
          const completion = await response.json(); const choice = completion.choices?.[0];
          if (completion.status === "confirmation_required") throw new Error(`Operator confirmation required for intent ${completion.intent}: ${JSON.stringify(completion.review)}. The broker retained the exact action without dispatching it.`);
          if (!choice?.message || !["stop", "length", "tool_calls"].includes(choice.finish_reason)) throw new Error("Broker returned an incomplete model receipt");
          costMicrousd = completion.swarm_cost_microusd;
          const delta = { ...choice.message }; delete delta.role;
          if (delta.tool_calls) delta.tool_calls = delta.tool_calls.map((call: object, index: number) => ({ ...call, index }));
          const frame = { id: completion.id, object: "chat.completion.chunk", created: completion.created, model: completion.model };
          const first = { ...frame, choices: [{ index: 0, delta, finish_reason: null }] };
          const last = { ...frame, choices: [{ index: 0, delta: {}, finish_reason: choice.finish_reason }], usage: completion.usage };
          return new Response(`data: ${JSON.stringify(first)}\n\ndata: ${JSON.stringify(last)}\n\ndata: [DONE]\n\n`, { headers: { "Content-Type": "text/event-stream" } });
        },
      });
      return lazyStream(model, async () => (async function* () {
        for await (const event of upstream) {
          if (event.type === "done" && Number.isSafeInteger(costMicrousd) && (costMicrousd ?? -1) >= 0) event.message.usage.cost.total = costMicrousd! / 1_000_000;
          yield event;
        }
      })());
    },
  });
  const tools = session.tools.map((tool: { name: string; description: string; parameters: object }) => ({
    name: tool.name, label: tool.name, description: tool.description, parameters: Type.Unsafe(tool.parameters),
  }));
  for (const tool of tools) pi.registerTool({ ...tool,
    async execute(id, params, signal) {
      const response = await broker(`/tool/${tool.name}`, params, signal, `tool-${id}`);
      if (!response.ok) throw new Error(`Broker denied ${tool.name}: ${await response.text()}`);
      const result = await response.json(); return { content: [{ type: "text", text: JSON.stringify(result) }], details: result };
    },
  });
  pi.on("session_start", () => pi.setActiveTools(tools.map((tool: { name: string }) => tool.name)));
  pi.on("tool_call", (event) => {
    if (!tools.some((tool: { name: string }) => tool.name === event.toolName)) return { block: true, reason: "Tool is outside the compiled definition" };
  });
}
