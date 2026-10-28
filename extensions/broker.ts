import { request } from "node:http";
import { Type, lazyStream, openAICompletionsApi } from "@earendil-works/pi-ai/compat";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const BOARD_MESSAGE_LIMIT = 2_048;

async function broker(path: string, payload: unknown, signal?: AbortSignal): Promise<Response> {
  const socketPath = process.env.SWARM_SOCKET;
  const token = process.env.SWARM_TOKEN;
  if (!socketPath || !token) throw new Error("Missing broker session");
  const body = JSON.stringify(payload);
  return new Promise((resolve, reject) => {
    const req = request({
      socketPath, path, method: "POST", signal,
      headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json", "Content-Length": Buffer.byteLength(body) },
    }, (res) => {
      const chunks: Buffer[] = [];
      let size = 0;
      res.on("data", (chunk: Buffer) => {
        size += chunk.length;
        if (size > 4 * 1024 * 1024) req.destroy(new Error("Broker response exceeds 4 MiB"));
        else chunks.push(chunk);
      });
      res.on("error", reject);
      res.on("end", () => resolve(new Response(Buffer.concat(chunks), {
        status: res.statusCode ?? 500,
        headers: { "Content-Type": "application/json" },
      })));
    });
    req.setTimeout(660_000, () => req.destroy(new Error("Broker request timed out")));
    req.on("error", reject);
    req.end(body);
  });
}

export default function swarmBroker(pi: ExtensionAPI) {
  pi.registerProvider("swarm", {
    baseUrl: "http://swarm.invalid/v1", api: "swarm-broker", apiKey: "$SWARM_TOKEN",
    models: [{
      id: "deepseek-flash", name: "Brokered DeepSeek Flash", reasoning: false,
      // The broker refuses requests over the Goal's max_input_bytes (1 MiB by default, roughly
      // 250k tokens). Advertising a smaller window makes pi compact before that refusal.
      input: ["text"], contextWindow: Number(process.env.SWARM_CONTEXT_TOKENS ?? 200_000), maxTokens: 65_536,
      cost: { input: 0.3, output: 1.2, cacheRead: 0.006, cacheWrite: 0 },
      compat: { supportsStore: false, supportsDeveloperRole: false, maxTokensField: "max_tokens", supportsStrictMode: false },
    }],
    streamSimple(model, context, options) {
      let costMicrousd: number | undefined;
      const upstream = openAICompletionsApi().streamSimple({ ...model, api: "openai-completions" }, context, {
        ...options, maxRetries: 0, timeoutMs: 86_400_000, // a paused broker may keep a call waiting for hours; the SDK default is 10 min
        fetch: async (input, init) => {
          const incoming = new Request(input, init);
          const payload = await incoming.json();
          let response = await broker("/model", { ...payload, stream: false }, incoming.signal);
          // A paused broker (operator pause or peak window) refuses BEFORE reserving anything: wait and ask again, as
          // many times as it takes. This is not an error, spends no call and no budget.
          while (response.status === 503) {
            const note = await response.clone().json().catch(() => ({}));
            if (!note.paused) break;
            const wait = Math.min(Math.max(Number(note.retry_after_s) || 30, 0.1), 300) * 1000;
            await new Promise((done, fail) => {
              const timer = setTimeout(done, wait);
              incoming.signal.addEventListener("abort", () => { clearTimeout(timer); fail(new Error("aborted while paused")); }, { once: true });
            });
            response = await broker("/model", { ...payload, stream: false }, incoming.signal);
          }
          if (!response.ok) return response;
          const completion = await response.json();
          const choice = completion.choices?.[0];
          if (!choice?.message || !["stop", "length", "tool_calls"].includes(choice.finish_reason)) {
            return new Response(JSON.stringify({ error: { message: "Invalid broker model response" } }), {
              status: 502, headers: { "Content-Type": "application/json" },
            });
          }
          costMicrousd = completion.swarm_cost_microusd;
          const delta = { ...choice.message };
          delete delta.role;
          if (delta.tool_calls) delta.tool_calls = delta.tool_calls.map((call: object, index: number) => ({ ...call, index }));
          const frame = { id: completion.id, object: "chat.completion.chunk", created: completion.created, model: completion.model };
          const data = { ...frame, choices: [{ index: 0, delta, finish_reason: null }] };
          const end = { ...frame, choices: [{ index: 0, delta: {}, finish_reason: choice.finish_reason }], usage: completion.usage };
          return new Response(`data: ${JSON.stringify(data)}\n\ndata: ${JSON.stringify(end)}\n\ndata: [DONE]\n\n`, {
            headers: { "Content-Type": "text/event-stream" },
          });
        },
      });
      return lazyStream(model, async () => (async function* () {
        for await (const event of upstream) {
          if (event.type === "done" && typeof costMicrousd === "number" && Number.isSafeInteger(costMicrousd) && costMicrousd >= 0) {
            event.message.usage.cost.total = costMicrousd / 1_000_000;
          }
          yield event;
        }
      })());
    },
  });

  const tools = [
    { name: "claim_task", description: "Claim your assigned task. Returns its ID and input payload. Claim exactly once before any other tool.", parameters: Type.Object({}) },
    { name: "board_read", description: "Read shared progress messages for this Goal.", parameters: Type.Object({}) },
    // Deliberately no maxLength here: a schema rejection is a tool error the
    // model can retry forever. The boundary shortens the message instead and
    // tells the model to continue.
    { name: "board_post", description: "Post a progress message to this Goal's shared Board. Messages longer than 2048 characters are shortened.", parameters: Type.Object({ message: Type.String({ minLength: 1 }) }) },
    { name: "submit_finding", description: "Submit your claimed task's structured result exactly as the Goal's worker prompt defines it. Immutable once accepted.", parameters: Type.Object({ task_id: Type.String(), result: Type.Object({}, { additionalProperties: true }) }) },
    { name: "read_findings", description: "Coordinator only: read completed findings from every worker.", parameters: Type.Object({}) },
    { name: "submit_artifact", description: "Coordinator only: submit the combined artifact exactly as the Goal's synthesis prompt defines it.", parameters: Type.Object({ artifact: Type.Object({}, { additionalProperties: true }) }) },
    { name: "fetch_url", description: "Fetch one live http(s) URL through the broker; returns status, headers and bounded body. Your only network egress.", parameters: Type.Object({ url: Type.String({ minLength: 1, maxLength: 2048 }), method: Type.Optional(Type.String()), headers: Type.Optional(Type.Object({}, { additionalProperties: true })), body: Type.Optional(Type.String()), max_bytes: Type.Optional(Type.Number()) }) },
    { name: "run_code", description: "Run one bash command in an isolated, CPU-only, offline Linux sandbox. The working directory and only writable path is /work. Read-only: /dataset (decoded Parquet), /regimes (source checkout), /venv (Python environment; use /venv/bin/python). No network, broker socket, host home, GPU, or credentials. Files are written into /work before the command. Returns bounded {exit_code,stdout,stderr,timed_out,duration_ms,truncated}. timeout_seconds is 1-300 (default 120).", parameters: Type.Object({ code: Type.String({ minLength: 1 }), files: Type.Optional(Type.Object({}, { additionalProperties: Type.String() })), timeout_seconds: Type.Optional(Type.Integer({ minimum: 1, maximum: 300 })), stdin: Type.Optional(Type.String()) }) },
  ].filter((tool) => (process.env.SWARM_ROLE === "coordinator" || !["read_findings", "submit_artifact"].includes(tool.name))
    && (!process.env.SWARM_GRANTS || process.env.SWARM_GRANTS.split(",").map((grant) => grant.trim()).filter(Boolean).includes(tool.name)));
  for (const tool of tools) pi.registerTool({
    ...tool, label: tool.name,
    async execute(_id, params, signal) {
      let payload = params;
      let notice: string | undefined;
      const message = (params as { message?: unknown }).message;
      if (tool.name === "board_post" && typeof message === "string" && message.length > BOARD_MESSAGE_LIMIT) {
        payload = { ...(params as Record<string, unknown>), message: message.slice(0, BOARD_MESSAGE_LIMIT) };
        notice = `Message shortened to the ${BOARD_MESSAGE_LIMIT}-character Board limit; continue.`;
      }
      const response = await broker(`/tool/${tool.name}`, payload, signal);
      if (!response.ok) throw new Error(`Broker denied ${tool.name}: HTTP ${response.status}`);
      const result = await response.json();
      if (notice && result && typeof result === "object") result.notice = notice;
      return { content: [{ type: "text", text: JSON.stringify(result) }], details: result };
    },
  });
  pi.on("session_start", () => pi.setActiveTools(tools.map((tool) => tool.name)));
  pi.on("tool_call", (event) => {
    if (!tools.some((tool) => tool.name === event.toolName)) return { block: true, reason: "Tool is outside the swarm grants" };
  });
}
