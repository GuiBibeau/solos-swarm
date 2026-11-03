import { request } from "node:http";
import { StringEnum, Type, lazyStream, openAICompletionsApi } from "@earendil-works/pi-ai/compat";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const BOARD_MESSAGE_LIMIT = 2_048;

// Agent mode's Router. It decides and reads; it has no run_code and no fetch_url.
const ROUTER_TOOLS = [
  {
    name: "router_decide",
    description: "Decide one candidate by its eval id: accept it as work, ignore it, or stop one of the Tickets on its thread. Call it once per candidate. A refused decision comes back as an error that says why; correct the call and try again.",
    parameters: Type.Object({
      eval: Type.String({ minLength: 1, description: "The eval id on the candidate's first line." }),
      decision: StringEnum(["accept", "ignore", "stop"] as const),
      reason: Type.String({ minLength: 1, description: "One sentence: why." }),
      title: Type.Optional(Type.String({ description: "accept: a short title for the work." })),
      scope: Type.Optional(Type.String({ description: "accept: what the worker must do, in your words and complete on its own." })),
      plan: Type.Optional(StringEnum(["session", "swarm"] as const, { description: "accept: session (one worker, the default) or swarm (wide research and tournaments only)." })),
      agents: Type.Optional(Type.Integer({ minimum: 1, maximum: 30, description: "accept with plan swarm: how many agents." })),
      environment: Type.Optional(Type.String({ description: "accept with plan session: the machine its run_code runs on, one your instructions name. Omit it for the default." })),
      budget_microusd: Type.Optional(Type.Integer({ minimum: 1, description: "accept: the Ticket's whole budget in microusd. Omit it for the default." })),
      acknowledge: Type.Optional(Type.String({ description: "accept: the first reply, instead of the standard one. An empty string sends none." })),
      rules: Type.Optional(Type.Array(Type.String(), { description: "accept of inferred work: the proactive rules it matches." })),
      ticket: Type.Optional(Type.String({ description: "stop: the id of the Ticket to stop." })),
    }),
  },
  {
    name: "router_read",
    description: "Read one trigger, thread or ticket by id. The answer names its neighbours by id; follow them one hop at a time when a candidate needs more context.",
    parameters: Type.Object({
      id: Type.String({ minLength: 1 }),
      kind: Type.Optional(StringEnum(["trigger", "thread", "ticket"] as const, { description: "Which kind of id it is, when it could be more than one." })),
    }),
  },
];

const FETCH_URL = { name: "fetch_url", description: "Fetch one live http(s) URL through the broker; returns status, headers and bounded body. Your only network egress.", parameters: Type.Object({ url: Type.String({ minLength: 1, maxLength: 2048 }), method: Type.Optional(Type.String()), headers: Type.Optional(Type.Object({}, { additionalProperties: true })), body: Type.Optional(Type.String()), max_bytes: Type.Optional(Type.Number()) }) };

// Agent mode's worker: one Ticket. It reports instead of talking to people; SWARM_GRANTS narrows the set.
const WORKER_TOOLS = [
  {
    name: "worker_report",
    description: "Report on your ticket. progress: still working, nobody is told. waiting: your turn is over and nothing is outstanding. ready: the result, sent to whoever asked; it ends the ticket. blocked or failed: you need a person; say why. dropped: nothing to do; ends the ticket silently. A refused report comes back as an error that says why.",
    parameters: Type.Object({
      status: StringEnum(["progress", "waiting", "ready", "blocked", "failed", "dropped"] as const),
      text: Type.String({ minLength: 1, description: "For ready, the complete result for the person who asked; otherwise what happened, briefly." }),
      question: Type.Optional(Type.String({ description: "One concrete question for a person, when only a person can answer it." })),
    }),
  },
  {
    name: "worker_handback",
    description: "Hand back a new message that reached your ticket but is not yours to handle: a request beyond your scope, other work, or a stop. The Router judges it; do not act on it yourself.",
    parameters: Type.Object({
      trigger: Type.String({ minLength: 1, description: "The trigger id of the message." }),
      reason: Type.String({ minLength: 1, description: "One sentence: why it is not yours." }),
    }),
  },
  { name: "run_code", description: "Run one bash command in an isolated, offline Linux sandbox. /work is your ticket's directory: the only writable path, kept between calls. Other paths the operator mounted are read-only. No network, broker socket, host home or credentials. Files are written into /work before the command. Returns bounded {exit_code,stdout,stderr,timed_out,duration_ms,truncated}. timeout_seconds is 1-300 (default 120). A command still running after a few seconds moves to the background and returns {job,status:\"running\",note}: use job_wait or job_stop on it, or end your turn and its result comes back as new input.", parameters: Type.Object({ code: Type.String({ minLength: 1 }), files: Type.Optional(Type.Object({}, { additionalProperties: Type.String() })), timeout_seconds: Type.Optional(Type.Integer({ minimum: 1, maximum: 300 })), stdin: Type.Optional(Type.String()) }) },
  {
    name: "job_start",
    description: "Start one bash command in the background, in run_code's sandbox with /work as its directory, and return {job,status} at once. Keep working or end your turn: when it ends after your turn, its result comes back to you as new input. Each byte of its output reaches you once, so write long output to a file in /work. A ticket holds a few jobs at a time.",
    parameters: Type.Object({
      code: Type.String({ minLength: 1 }),
      timeout_s: Type.Optional(Type.Integer({ minimum: 1, description: "Kill it after this many seconds. Default and most: the operator's job limit (30 minutes unless set)." })),
    }),
  },
  {
    name: "job_wait",
    description: "Wait up to wait_s for a job. Returns its end ({job,status,exit_code,stdout,stderr,...}), or {status:\"running\"} with what it printed since you last looked. It returns early when new input waits for you; end your turn to read it.",
    parameters: Type.Object({
      job: Type.String({ minLength: 1, description: "The job id from job_start or run_code." }),
      wait_s: Type.Optional(Type.Integer({ minimum: 0, maximum: 120, description: "Seconds to wait; 0 only looks. Default 30." })),
    }),
  },
  {
    name: "job_stop",
    description: "Kill a job and return its end with what it printed that you had not read.",
    parameters: Type.Object({ job: Type.String({ minLength: 1 }) }),
  },
  FETCH_URL,
];

function granted(name: string): boolean {
  return !process.env.SWARM_GRANTS || process.env.SWARM_GRANTS.split(",").map((grant) => grant.trim()).filter(Boolean).includes(name);
}

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
  // Unset is a swarm agent. Anything but a known set is refused, so a typo cannot widen the tools.
  const toolset = process.env.SWARM_TOOLSET;
  if (toolset !== undefined && toolset !== "router" && toolset !== "worker") throw new Error(`Unknown SWARM_TOOLSET: ${toolset}`);
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

  const tools = toolset === "router" ? ROUTER_TOOLS : toolset === "worker" ? WORKER_TOOLS.filter((tool) => granted(tool.name)) : [
    { name: "claim_task", description: "Claim your assigned task. Returns its ID and input payload. Claim exactly once before any other tool.", parameters: Type.Object({}) },
    { name: "board_read", description: "Read shared progress messages for this Goal.", parameters: Type.Object({}) },
    // Deliberately no maxLength here: a schema rejection is a tool error the
    // model can retry forever. The boundary shortens the message instead and
    // tells the model to continue.
    { name: "board_post", description: "Post a progress message to this Goal's shared Board. Messages longer than 2048 characters are shortened.", parameters: Type.Object({ message: Type.String({ minLength: 1 }) }) },
    { name: "submit_finding", description: "Submit your claimed task's structured result exactly as the Goal's worker prompt defines it. Immutable once accepted.", parameters: Type.Object({ task_id: Type.String(), result: Type.Object({}, { additionalProperties: true }) }) },
    { name: "read_findings", description: "Coordinator only: read completed findings from every worker.", parameters: Type.Object({}) },
    { name: "submit_artifact", description: "Coordinator only: submit the combined artifact exactly as the Goal's synthesis prompt defines it.", parameters: Type.Object({ artifact: Type.Object({}, { additionalProperties: true }) }) },
    FETCH_URL,
    { name: "run_code", description: "Run one bash command in an isolated, CPU-only, offline Linux sandbox. The working directory and only writable path is /work. Read-only: /dataset (decoded Parquet), /regimes (source checkout), /venv (Python environment; use /venv/bin/python). No network, broker socket, host home, GPU, or credentials. Files are written into /work before the command. Returns bounded {exit_code,stdout,stderr,timed_out,duration_ms,truncated}. timeout_seconds is 1-300 (default 120).", parameters: Type.Object({ code: Type.String({ minLength: 1 }), files: Type.Optional(Type.Object({}, { additionalProperties: Type.String() })), timeout_seconds: Type.Optional(Type.Integer({ minimum: 1, maximum: 300 })), stdin: Type.Optional(Type.String()) }) },
  ].filter((tool) => (process.env.SWARM_ROLE === "coordinator" || !["read_findings", "submit_artifact"].includes(tool.name))
    && granted(tool.name));
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
      if (!response.ok && toolset !== undefined) {
        // An agent session corrects a refused call from the Engine's reason, so the reason is the tool error.
        const refusal = await response.json().catch(() => ({}));
        throw new Error(`Broker refused ${tool.name}: ${refusal.error ?? `HTTP ${response.status}`}`);
      }
      if (!response.ok) throw new Error(`Broker denied ${tool.name}: HTTP ${response.status}`);
      const result = await response.json();
      if (notice && result && typeof result === "object") result.notice = notice;
      return { content: [{ type: "text", text: JSON.stringify(result) }], details: result };
    },
  });
  pi.on("session_start", () => pi.setActiveTools(tools.map((tool) => tool.name)));
  if (toolset !== undefined) {
    // The host writes an agent session's instructions; the session fetches them from its broker once.
    let systemPrompt: string | undefined;
    pi.on("before_agent_start", async () => {
      if (systemPrompt === undefined) {
        const response = await broker("/session", {});
        if (!response.ok) throw new Error(`Broker refused the session: HTTP ${response.status}`);
        const session = await response.json();
        if (typeof session.system_prompt !== "string") throw new Error("Broker sent no system prompt");
        systemPrompt = session.system_prompt;
      }
      return { systemPrompt };
    });
  }
  pi.on("tool_call", (event) => {
    if (!tools.some((tool) => tool.name === event.toolName)) return { block: true, reason: "Tool is outside the swarm grants" };
  });
}
