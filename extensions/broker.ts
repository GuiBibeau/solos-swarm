import { request } from "node:http";
import { Type, lazyStream, openAICompletionsApi } from "@earendil-works/pi-ai/compat";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

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
    req.setTimeout(120_000, () => req.destroy(new Error("Broker request timed out")));
    req.on("error", reject);
    req.end(body);
  });
}

export default function swarmBroker(pi: ExtensionAPI) {
  pi.registerProvider("swarm", {
    baseUrl: "http://swarm.invalid/v1", api: "swarm-broker", apiKey: "$SWARM_TOKEN",
    models: [{
      id: "deepseek-flash", name: "Brokered DeepSeek Flash", reasoning: false,
      input: ["text"], contextWindow: 128_000, maxTokens: 4096,
      cost: { input: 0.3, output: 1.2, cacheRead: 0.03, cacheWrite: 0 },
      compat: { supportsStore: false, supportsDeveloperRole: false, maxTokensField: "max_tokens", supportsStrictMode: false },
    }],
    streamSimple(model, context, options) {
      let costMicrousd: number | undefined;
      const upstream = openAICompletionsApi().streamSimple({ ...model, api: "openai-completions" }, context, {
        ...options, maxRetries: 0,
        fetch: async (input, init) => {
          const incoming = new Request(input, init);
          const payload = await incoming.json();
          const response = await broker("/model", { ...payload, stream: false }, incoming.signal);
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
    { name: "claim_task", description: "Claim your assigned evidence-checking task. Returns its ID and source records.", parameters: Type.Object({}) },
    { name: "board_read", description: "Read shared progress messages for this Goal.", parameters: Type.Object({}) },
    { name: "board_post", description: "Post a progress message to this Goal's shared Board.", parameters: Type.Object({ message: Type.String({ minLength: 1, maxLength: 2048 }) }) },
    { name: "submit_finding", description: "Submit your claimed task's findings. Use result.inconsistencies with objects containing id, source_a and source_b.", parameters: Type.Object({ task_id: Type.String(), result: Type.Object({}, { additionalProperties: true }) }) },
    { name: "read_findings", description: "Coordinator only: read completed findings from every worker.", parameters: Type.Object({}) },
    { name: "submit_artifact", description: "Coordinator only: submit the combined artifact with inconsistencies from all ten tasks.", parameters: Type.Object({ artifact: Type.Object({}, { additionalProperties: true }) }) },
  ].filter((tool) => process.env.SWARM_ROLE === "coordinator" || !["read_findings", "submit_artifact"].includes(tool.name));
  for (const tool of tools) pi.registerTool({
    ...tool, label: tool.name,
    async execute(_id, params, signal) {
      const response = await broker(`/tool/${tool.name}`, params, signal);
      if (!response.ok) throw new Error(`Broker denied ${tool.name}: HTTP ${response.status}`);
      const result = await response.json();
      return { content: [{ type: "text", text: JSON.stringify(result) }], details: result };
    },
  });
  pi.on("session_start", () => pi.setActiveTools(tools.map((tool) => tool.name)));
  pi.on("tool_call", (event) => {
    if (!tools.some((tool) => tool.name === event.toolName)) return { block: true, reason: "Tool is outside the swarm grants" };
  });
}
