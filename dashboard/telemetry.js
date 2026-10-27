(function (root) {
  "use strict";

  function record(value) { return value && typeof value === "object" && !Array.isArray(value) ? value : null; }
  function number(value) { return typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : null; }
  function text(value) { return typeof value === "string" && value.trim() ? value : null; }
  function status(value) {
    if (typeof value === "string") return value;
    var object = record(value);
    return object && Object.keys(object).length === 1 ? Object.keys(object)[0] : null;
  }
  function statusKey(value) { return (status(value) || "").replace(/[\s_-]/g, "").toLowerCase(); }
  function sumKnown(items, read) {
    var values = items.map(read);
    return values.every(function (value) { return value !== null; }) ? values.reduce(function (sum, value) { return sum + value; }, 0) : null;
  }
  function completed(intent) { var key = statusKey(intent.state); return key === "completed" || key === "settled"; }
  function cost(intent) {
    var value = number(intent.cost_microusd);
    return value !== null ? value : completed(intent) ? null : 0;
  }
  function tokens(intent) {
    if (!completed(intent)) return 0;
    var usage = record(intent.usage);
    if (!usage) return null;
    var input = number(usage.input_tokens), output = number(usage.output_tokens);
    return input !== null && output !== null ? input + output : null;
  }

  function normalize(raw) {
    if (!record(raw)) return null;
    var envelope = raw.schema === "solos-swarm.telemetry/v1";
    var legacy = raw.schema_version !== undefined && record(raw.agents) && Array.isArray(raw.tasks);
    if (!envelope && !legacy) return null;
    var state = envelope ? record(raw.state) || {} : raw;
    var config = record(envelope ? raw.config : raw.configuration) || {};
    var rawSource = record(raw.source) || {};
    var source = {
      kind: text(rawSource.kind) || "snapshot",
      label: text(rawSource.label) || "Snapshot",
      updated_at: number(rawSource.updated_at),
      connected: rawSource.connected === true,
      service_active: typeof rawSource.service_active === "boolean" ? rawSource.service_active : null
    };
    var agents = record(state.agents) || {};
    var ids = Object.keys(agents);
    var tasks = Array.isArray(state.tasks) ? state.tasks.filter(record) : [];
    var intents = Array.isArray(state.intents) ? state.intents.filter(record) : null;
    var findings = Array.isArray(state.findings) ? state.findings.filter(record) : [];
    var goalStatus = status(state.goal_status);
    var running = statusKey(goalStatus) === "running";
    var label = goalStatus ? goalStatus.replace(/([a-z])([A-Z])/g, "$1 $2").replace(/[_-]+/g, " ").toUpperCase() : "UNKNOWN";
    if (running && source.service_active === false) label = "STOPPED";
    var canAdvance = running && source.service_active === true && source.updated_at !== null;

    var workers = ids.map(function (id, index) {
      var agent = record(agents[id]) || {};
      var task = tasks.find(function (item) { return item.claimed_by === id; });
      var owned = intents === null ? null : intents.filter(function (intent) { return intent.agent_id === id; });
      var finding = findings.find(function (item) { return item.agent_id === id || (task && item.task_id === task.id); });
      var start = number(agent.started_ms), end = number(agent.stopped_ms);
      if (end === null && canAdvance && statusKey(agent.status) === "running") end = source.updated_at;
      var duration = start !== null && end !== null && end >= start ? end - start : null;
      var calls = number(agent.model_calls);
      var recorded = finding && (finding.has_result === true || (finding.result !== undefined && finding.result !== null));
      return {
        index: index,
        workerId: id,
        taskId: task ? text(task.id) : null,
        fence: task ? number(task.fencing_token) : null,
        state: status(agent.status),
        durationMs: duration,
        calls: calls,
        tokens: owned === null ? null : sumKnown(owned, tokens),
        settledMicro: owned === null ? null : sumKnown(owned, cost),
        findingSummary: finding && text(finding.summary) || (recorded ? "Finding recorded" : "No finding recorded")
      };
    });

    var starts = ids.map(function (id) { return number((record(agents[id]) || {}).started_ms); });
    var stops = ids.map(function (id) { return number((record(agents[id]) || {}).stopped_ms); }).filter(function (value) { return value !== null; });
    var end = stops.length ? Math.max.apply(null, stops) : null;
    if (canAdvance && stops.length < ids.length) end = source.updated_at;
    var totalMs = null;
    if (starts.length && starts.every(function (value) { return value !== null; }) && end !== null) {
      var start = Math.min.apply(null, starts);
      if (end >= start) totalMs = end - start;
    }
    var durations = workers.map(function (worker) { return worker.durationMs; }).filter(function (value) { return value !== null; });
    var perRow = Math.max(1, Math.ceil(workers.length / 10));
    var timelineRows = [];
    for (var index = 0; index < workers.length; index += perRow) {
      var group = workers.slice(index, index + perRow);
      var known = group.map(function (worker) { return worker.durationMs; }).filter(function (value) { return value !== null; });
      timelineRows.push({
        rowIndex: timelineRows.length,
        label: group.length === 1 ? group[0].workerId : group[0].workerId + "–" + group[group.length - 1].workerId,
        durationMs: known.length ? Math.max.apply(null, known) : null,
        synthesisMs: 0,
        hasSynthesis: false,
        workerIndices: group.map(function (worker) { return worker.index; })
      });
    }
    var calls = sumKnown(ids, function (id) { return number((record(agents[id]) || {}).model_calls); });
    return {
      isRealData: true,
      source: source,
      goalId: text(config.goal_id) || source.label,
      goalTitle: text(config.intent) || "Swarm telemetry",
      statusLabel: label,
      workerCount: workers.length,
      configuredWorkers: number(config.max_agents),
      completedShards: tasks.filter(function (task) { var key = statusKey(task.status === undefined ? task.state : task.status); return key === "done" || key === "completed"; }).length,
      keysInWorkers: number(config.keys_in_workers),
      ceilingMicro: number(config.budget_microusd),
      settledMicro: number(state.spent_microusd),
      heldMicro: number(state.held_microusd),
      unknownHolds: intents === null ? null : intents.filter(function (intent) { var key = statusKey(intent.state); return key === "unknown" || key === "unknownhold"; }).length,
      modelCalls: calls,
      totalTokens: intents === null ? null : sumKnown(intents, tokens),
      totalMs: totalMs,
      parallelMs: durations.length ? Math.max.apply(null, durations) : null,
      synthMs: null,
      revision: number(raw.revision) !== null ? raw.revision : number(state.revision),
      workers: workers,
      timelineRows: timelineRows,
      daemons: [],
      model: text(config.model),
      provider: text(config.provider),
      capabilities: Array.isArray(config.capabilities) ? config.capabilities.filter(function (item) { return typeof item === "string"; }) : null
    };
  }

  root.SwarmTelemetry = { normalize: normalize };
})(typeof window !== "undefined" ? window : globalThis);
