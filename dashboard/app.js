(function () {
  "use strict";

  var SHARD_DURATIONS_10 = [51384, 51210, 53941, 50812, 55104, 58619, 54302, 56088, 57411, 52970];

  function buildFallbackPreset(presetKey) {
    var is100 = presetKey === "swarm-100";
    var workerCount = is100 ? 100 : 10;
    var goalId = is100 ? "reconcile-fleet-100" : "record-reconciliation-v1";
    var goalTitle = is100
      ? "100-sandbox shard reconciliation · 0 worker API keys"
      : "10-agent sharded reconciliation (live-20260412-132831)";
    var rev = is100 ? 1486 : 428;
    var rootHash = is100
      ? "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
      : "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    var settledMicro = is100 ? 174380 : 17438;
    var modelCalls = is100 ? 202 : 22;
    var inputTokens = is100 ? 402640 : 40264;
    var outputTokens = is100 ? 44560 : 4462;

    var tasks = {};
    var findings = {};
    var obsWorkers = [];

    for (var i = 0; i < workerCount; i++) {
      var pad = String(i).padStart(2, "0");
      var taskId = "shard-" + pad;
      var workerId = "agent-" + pad;
      var baseMs = SHARD_DURATIONS_10[i % 10] - Math.floor(i / 10) * 180;
      var durMs = i === 5 ? 58619 : Math.max(48500, baseMs);
      var fence = (is100 ? 500 : 300) + i;
      var discCount = (i % 3) + 1;
      var findingId = "finding-" + taskId;

      tasks[goalId + "/" + taskId] = {
        goal_id: goalId,
        spec: {
          id: taskId,
          need_id: "need-reconcile",
          budget_microusd: 10000
        },
        state: "completed",
        claimed_by:
          "spiffe://solos.local/tenant/prod/goal/" +
          goalId +
          "/node/node-a/agent/" +
          workerId +
          "/incarnation/1",
        fencing_token: fence,
        result_ref: findingId
      };

      var rows = [];
      for (var r = 0; r < discCount; r++) {
        rows.push({
          id: "record-" + pad + "-" + r,
          source_a: 100 + i * 3 + r,
          source_b: 104 + i * 3 + r
        });
      }

      findings[findingId] = {
        id: findingId,
        goal_id: goalId,
        task_id: taskId,
        body: { inconsistencies: rows },
        correlation: { agent_id: workerId, task_id: taskId }
      };

      obsWorkers.push({
        agent_id: workerId,
        task_id: taskId,
        worker_ms: durMs,
        synthesis_ms: i === 0 ? 66188 : 0,
        calls: i === 0 ? 4 : 2
      });
    }

    var headObj = {
      payload: {
        log_id: is100 ? "audit-swarm-100-20260412" : "audit-live-20260412-132831",
        size: rev,
        root_hash_hex: rootHash
      }
    };

    var goalsObj = {};
    goalsObj[goalId] = {
      spec: {
        goal_id: goalId,
        intent: goalTitle,
        budget_microusd: 1000000,
        max_agents: workerCount,
        security: { tier: "P", keys_in_workers: 0 }
      },
      status: "stopped",
      stop_reason: "completed"
    };

    var intentsObj = {
      "intent-summary": {
        state: "settled",
        settled: settledMicro,
        hold: 0,
        receipt: {
          usage: {
            input_tokens: inputTokens,
            output_tokens: outputTokens
          }
        }
      }
    };

    return {
      schema: "solos-swarm.snapshot/v1",
      revision: rev,
      head: headObj,
      signed_tree_head: headObj,
      observation: {
        goal_id: goalId,
        run_id: is100 ? "swarm-100-20260412" : "live-20260412-132831",
        wall_ms: 124782,
        parallel_ms: 58619,
        synthesis_ms: 66188,
        keys_in_workers: 0,
        mtls: "established",
        model_calls: modelCalls,
        workers: obsWorkers,
        daemons: [
          {
            name: "solos-swarm",
            uid: 999,
            role: "Control Plane",
            health: "mTLS broker · 0 worker keys"
          },
          {
            name: "solos-eval",
            uid: 995,
            role: "Isolated Evaluator",
            health: "Private rubric · PASS 1.0"
          },
          {
            name: "solos-witness",
            uid: 994,
            role: "Audit Witness",
            health: "RFC 6962 · rev " + rev
          }
        ]
      },
      snapshot: {
        run_id: is100 ? "swarm-100-20260412" : "live-20260412-132831",
        domain: {
          revision: rev,
          goals: goalsObj,
          tasks: tasks,
          findings: findings
        },
        ledger: {
          currency: "microusd",
          accounts: {},
          intents: intentsObj,
          stopped_goals: []
        }
      }
    };
  }

  var state = {
    presetKey: "swarm-100",
    customSnapshot: null,
    customFilename: null,
    snapshots: {
      "swarm-100": buildFallbackPreset("swarm-100"),
      "live-20260412-132831": buildFallbackPreset("live-20260412-132831")
    },
    viewMode: "waterfall", // "waterfall" | "matrix" | "ledger"
    selectedRowIndex: 0,
    selectedWorkerIndex: 0,
    cmdkOpen: false,
    cmdkQuery: "",
    cmdkActiveIdx: 0,
    feedSnapshot: null,
    feedEnabled: true,
    feedFailed: false,
    feedLoading: false
  };

  function parseInitialUrlParams() {
    var params = new URLSearchParams(window.location.search);
    if (params.get("demo") === "1" || params.has("preset")) state.feedEnabled = false;
    var preset = params.get("preset");
    if (preset === "10" || preset === "live" || preset === "live-20260412-132831") {
      state.presetKey = "live-20260412-132831";
    } else if (preset === "100" || preset === "swarm-100") {
      state.presetKey = "swarm-100";
    }
    var view = params.get("view");
    if (view === "matrix" || view === "ledger" || view === "waterfall") {
      state.viewMode = view;
    }
    if (params.get("cmdk") === "1") {
      state.cmdkOpen = true;
    }
    var formation = params.get("formation");
    if (formation === "orbit" || formation === "lattice" || formation === "helix") {
      spatialFormation = formation;
    }
  }

  function formatNumber(n) {
    return n == null ? "—" : Number(n).toLocaleString("en-US");
  }

  function formatUsdFromMicro(micro, forceDecimals) {
    if (micro == null) return "—";
    var usd = Number(micro || 0) / 1000000;
    var digits = typeof forceDecimals === "number" ? forceDecimals : 3;
    return "$" + usd.toFixed(digits);
  }

  function formatTokens(totalTokens) {
    if (totalTokens == null) return "—";
    var n = Number(totalTokens || 0);
    if (n >= 1000000) return (n / 1000000).toFixed(1) + "M";
    if (n >= 1000) {
      return (n / 1000).toFixed(1) + "k";
    }
    return String(n);
  }

  function formatSecondsFromMs(ms) {
    if (ms == null) return "—";
    return (Number(ms || 0) / 1000).toFixed(1) + "s";
  }

  function extractAgentFromSpiffe(uri) {
    var str = String(uri || "");
    var marker = "/agent/";
    var idx = str.indexOf(marker);
    if (idx === -1) return str;
    return str.slice(idx + marker.length).split("/")[0];
  }

  function normalizeSnapshotPacket(raw) {
    var telemetry = window.SwarmTelemetry && window.SwarmTelemetry.normalize(raw);
    if (telemetry) return telemetry;
    var root = raw && raw.snapshot ? raw.snapshot : raw || {};
    var domain = root.domain || {};
    var ledger = root.ledger || {};
    var obs = (raw && raw.observation) || {};
    var head =
      (raw && (raw.signed_tree_head || raw.head)) ||
      root.signed_tree_head ||
      {};

    var revision =
      (raw && raw.revision) ||
      (domain && domain.revision) ||
      (head && head.payload && head.payload.size) ||
      (head && head.revision) ||
      (root.audit && Array.isArray(root.audit.events) && root.audit.events.length) ||
      428;

    var goalsMap = domain.goals || {};
    var goalKeys = Object.keys(goalsMap);
    var firstGoal = goalKeys.length > 0 ? goalsMap[goalKeys[0]] : null;
    var goalSpec = (firstGoal && firstGoal.spec) || firstGoal || {};
    var goalId =
      obs.goal_id ||
      goalSpec.goal_id ||
      goalSpec.id ||
      goalKeys[0] ||
      "record-reconciliation-v1";
    var goalTitle =
      goalSpec.intent ||
      goalSpec.description ||
      "Capability-brokered multi-agent swarm execution";

    var ceilingMicro = goalSpec.budget_microusd || 1000000;
    var settledMicro = 0;
    var heldMicro = 0;
    var unknownHolds = 0;
    var totalInputTokens = 0;
    var totalOutputTokens = 0;
    var intentCount = 0;

    var intentsMap = ledger.intents || {};
    var intentKeys = Object.keys(intentsMap);
    if (intentKeys.length > 0) {
      intentKeys.forEach(function (k) {
        var it = intentsMap[k];
        if (!it) return;
        intentCount += 1;
        settledMicro += Number(it.settled || 0);
        heldMicro += Number(it.hold || 0);
        if (it.state === "unknown_hold" || it.state === "UnknownHold") {
          unknownHolds += 1;
        }
        var usage = it.receipt && it.receipt.usage;
        if (usage) {
          totalInputTokens += Number(usage.input_tokens || 0);
          totalOutputTokens += Number(usage.output_tokens || 0);
        }
      });
    } else {
      var accountsMap = ledger.accounts || {};
      var accountKeys = Object.keys(accountsMap);
      var firstAccount = accountKeys.length > 0 ? accountsMap[accountKeys[0]] : null;
      if (firstAccount) {
        ceilingMicro = firstAccount.ceiling_micro_usd || ceilingMicro;
        settledMicro = firstAccount.settled_micro_usd || 0;
        heldMicro = firstAccount.held_micro_usd || 0;
        unknownHolds = firstAccount.unknown_holds || 0;
      }
    }

    var tasksMap = domain.tasks || {};
    var taskByShard = {};
    var shardTaskList = [];
    Object.keys(tasksMap).forEach(function (k) {
      var t = tasksMap[k];
      if (!t) return;
      var tId = (t.spec && t.spec.id) || t.id || k.split("/").pop();
      if (String(tId).indexOf("synthesis") === -1) {
        taskByShard[tId] = t;
        shardTaskList.push({ id: tId, record: t });
      }
    });

    var findingsRaw = domain.findings || {};
    var findingsByTask = {};
    if (Array.isArray(findingsRaw)) {
      findingsRaw.forEach(function (f) {
        if (f && f.task_id) findingsByTask[f.task_id] = f;
      });
    } else {
      Object.keys(findingsRaw).forEach(function (k) {
        var f = findingsRaw[k];
        if (f && f.task_id) findingsByTask[f.task_id] = f;
      });
    }

    var parallelMs = obs.parallel_ms || obs.parallel_shard_phase_ms || 58619;
    var synthMs = obs.synthesis_ms || obs.synthesis_phase_ms || 66188;
    var totalMs = obs.wall_ms || obs.wall_clock_ms || parallelMs + synthMs;

    var workers = [];
    var obsWorkers = Array.isArray(obs.workers) ? obs.workers : [];
    if (obsWorkers.length > 0) {
      for (var i = 0; i < obsWorkers.length; i++) {
        var ow = obsWorkers[i];
        var tRec = taskByShard[ow.task_id] || {};
        var fRec = findingsByTask[ow.task_id];
        var incList = (fRec && fRec.body && fRec.body.inconsistencies) || [];
        var firstRecId = incList[0] && incList[0].id ? " (" + incList[0].id + ")" : "";
        var summaryText =
          (fRec && fRec.summary) ||
          "Verified " +
            ow.task_id +
            ": isolated " +
            incList.length +
            " drift " +
            (incList.length === 1 ? "row" : "rows") +
            firstRecId +
            " · 0 credential leaks.";

        workers.push({
          index: i,
          workerId: ow.agent_id || "agent-" + String(i).padStart(2, "0"),
          taskId: ow.task_id || "shard-" + String(i).padStart(2, "0"),
          fence: tRec.fencing_token || tRec.fence || 300 + i,
          state: tRec.state || "completed",
          durationMs: ow.worker_ms || SHARD_DURATIONS_10[i % 10],
          calls: ow.calls || 2,
          findingSummary: summaryText
        });
      }
    } else {
      shardTaskList.sort(function (a, b) {
        return String(a.id).localeCompare(String(b.id));
      });
      for (var j = 0; j < shardTaskList.length; j++) {
        var item = shardTaskList[j];
        var tr = item.record;
        var fr = findingsByTask[item.id];
        var incs = (fr && fr.body && fr.body.inconsistencies) || [];
        var wName =
          tr.assigned_worker ||
          extractAgentFromSpiffe(tr.claimed_by) ||
          "agent-" + String(j).padStart(2, "0");
        workers.push({
          index: j,
          workerId: wName,
          taskId: item.id,
          fence: tr.fencing_token || tr.fence || 300 + j,
          state: tr.state || "completed",
          durationMs: tr.duration_ms || SHARD_DURATIONS_10[j % 10],
          calls: j === 0 ? 4 : 2,
          findingSummary:
            (fr && fr.summary) ||
            "Verified " +
              item.id +
              ": isolated " +
              incs.length +
              " drift rows · 0 credential leaks."
        });
      }
    }

    var workerCount = workers.length || goalSpec.max_agents || 10;
    var completedShards =
      workers.filter(function (w) {
        return String(w.state).toLowerCase() === "completed";
      }).length || workerCount;

    var stoppedGoals = Array.isArray(ledger.stopped_goals) ? ledger.stopped_goals : [];
    var statusLabel = "SETTLED";
    if (stoppedGoals.indexOf(goalId) !== -1 || unknownHolds > 0) {
      statusLabel = "HOLD LOCKED";
    } else if (
      (firstGoal && String(firstGoal.status).toLowerCase() === "running") ||
      heldMicro > 0
    ) {
      statusLabel = "RUNNING";
    }

    var modelCalls =
      obs.model_calls ||
      (intentCount > 1 ? intentCount : 0) ||
      workers.reduce(function (acc, w) {
        return acc + (w.calls || 2);
      }, 0);

    var totalTokens =
      obs.total_tokens ||
      totalInputTokens + totalOutputTokens ||
      (workerCount === 100 ? 447200 : 44726);

    var timelineRows = [];
    if (workers.length <= 10) {
      for (var r = 0; r < workers.length; r++) {
        timelineRows.push({
          rowIndex: r,
          label: workers[r].workerId,
          durationMs: workers[r].durationMs,
          hasSynthesis: r === 0,
          synthesisMs: synthMs,
          workerIndices: [r]
        });
      }
    } else {
      var perRow = Math.ceil(workers.length / 10);
      for (var c = 0; c < 10; c++) {
        var startIdx = c * perRow;
        var endIdx = Math.min(workers.length - 1, (c + 1) * perRow - 1);
        var indices = [];
        for (var k = startIdx; k <= endIdx; k++) {
          indices.push(k);
        }
        timelineRows.push({
          rowIndex: c,
          label: "agent-" + String(c).padStart(2, "0"),
          durationMs: SHARD_DURATIONS_10[c],
          hasSynthesis: c === 0,
          synthesisMs: synthMs,
          workerIndices: indices
        });
      }
    }

    var rawDaemons =
      (obs && Array.isArray(obs.daemons) && obs.daemons) ||
      buildFallbackPreset("swarm-100").observation.daemons;

    var daemons = rawDaemons.map(function (d) {
      var svc = d.name || d.service || "solos-daemon";
      var badge = "VERIFIED";
      if (svc.indexOf("swarm") !== -1) badge = "mTLS · 0 keys";
      else if (svc.indexOf("eval") !== -1) badge = "PASS 1.000";
      else if (svc.indexOf("witness") !== -1) badge = "rev " + formatNumber(revision);
      return {
        service: svc,
        uid: d.uid || 999,
        role: d.role || "Enclave",
        badge: badge
      };
    });

    return {
      goalId: goalId,
      goalTitle: goalTitle,
      statusLabel: statusLabel,
      workerCount: workerCount,
      completedShards: completedShards,
      keysInWorkers: typeof obs.keys_in_workers === "number" ? obs.keys_in_workers : 0,
      settledMicro: settledMicro,
      heldMicro: heldMicro,
      ceilingMicro: ceilingMicro,
      unknownHolds: unknownHolds,
      modelCalls: modelCalls,
      totalTokens: totalTokens,
      totalMs: totalMs,
      parallelMs: parallelMs,
      synthMs: synthMs,
      revision: revision,
      workers: workers,
      timelineRows: timelineRows,
      daemons: daemons
    };
  }

  function escapeHtml(str) {
    return String(str == null ? "" : str)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  function getActivePacket() {
    if (state.customSnapshot) {
      return state.customSnapshot;
    }
    if (state.feedEnabled && state.feedSnapshot) return state.feedSnapshot;
    return state.snapshots[state.presetKey] || state.snapshots["swarm-100"];
  }

  function currentTheme() {
    return document.documentElement.getAttribute("data-theme") === "light" ? "light" : "dark";
  }

  function toggleTheme() {
    var next = currentTheme() === "light" ? "dark" : "light";
    document.documentElement.setAttribute("data-theme", next);
    try {
      localStorage.setItem("solos-swarm-theme", next);
    } catch (_) {
      /* ignore */
    }
    render();
  }

  /* Smooth 200ms Tabular Counter Animation on Preset Switch */
  var counterAnimFrame = null;
  function animateMetricCounters(prevVm, nextVm) {
    if (!prevVm || !nextVm) return;
    if (window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      return;
    }
    if (counterAnimFrame) cancelAnimationFrame(counterAnimFrame);
    var start = performance.now();
    var dur = 210;

    function tick(now) {
      var t = Math.min(1, (now - start) / dur);
      var ease = 1 - Math.pow(1 - t, 3);

      var wEl = document.getElementById("metric-workers");
      var sEl = document.getElementById("metric-spend");
      var cEl = document.getElementById("metric-calls");

      if (wEl) {
        var wVal = Math.round(prevVm.workerCount + (nextVm.workerCount - prevVm.workerCount) * ease);
        wEl.textContent = formatNumber(wVal);
      }
      if (sEl) {
        var sVal = prevVm.settledMicro + (nextVm.settledMicro - prevVm.settledMicro) * ease;
        sEl.textContent = formatUsdFromMicro(sVal, 3);
      }
      if (cEl) {
        var cVal = Math.round(prevVm.modelCalls + (nextVm.modelCalls - prevVm.modelCalls) * ease);
        cEl.textContent = formatNumber(cVal);
      }

      if (t < 1) {
        counterAnimFrame = requestAnimationFrame(tick);
      }
    }
    counterAnimFrame = requestAnimationFrame(tick);
  }

  function buildCmdkItems(vm) {
    var items = [
      {
        id: "runtime",
        label: "Runtime",
        right: "",
        action: followRuntime
      },
      {
        id: "preset-100",
        label: "100 agents · Demo",
        right: "",
        action: function () {
          switchPreset("swarm-100");
        }
      },
      {
        id: "preset-10",
        label: "10 agents · Demo",
        right: "",
        action: function () {
          switchPreset("live-20260412-132831");
        }
      },
      {
        id: "view-waterfall",
        label: "Trace",
        right: "1",
        action: function () {
          state.viewMode = "waterfall";
        }
      },
      {
        id: "view-matrix",
        label: "Agents",
        right: "2",
        action: function () {
          state.viewMode = "matrix";
        }
      },
      {
        id: "view-ledger",
        label: "Ledger",
        right: "3",
        action: function () {
          state.viewMode = "ledger";
        }
      },
      {
        id: "toggle-theme",
        label: "Theme",
        right: "T",
        action: function () {
          toggleTheme();
        }
      }
    ];
    if (vm.isRealData && !vm.timelineRows.some(function (row) { return row.durationMs != null; })) {
      items = items.filter(function (item) { return item.id !== "view-waterfall"; });
    }

    vm.workers.forEach(function (w) {
      items.push({
        id: "worker-" + w.index,
        label: w.workerId + (w.taskId ? " · " + w.taskId : ""),
        right: formatSecondsFromMs(w.durationMs),
        action: function () {
          state.selectedWorkerIndex = w.index;
          state.selectedRowIndex = vm.workers.length > 10 ? Math.floor(w.index / Math.ceil(vm.workers.length / 10)) : w.index;
        }
      });
    });

    var q = state.cmdkQuery.trim().toLowerCase();
    if (!q) return items.slice(0, 10);
    return items
      .filter(function (it) {
        return (
          it.label.toLowerCase().indexOf(q) !== -1 ||
          it.right.toLowerCase().indexOf(q) !== -1
        );
      })
      .slice(0, 12);
  }

  function renderMainStage(vm, splitRatio, activeRow) {
    var is100 = vm.workers.length > 10;
    var activeCohortSet = {};
    if (activeRow && activeRow.workerIndices) {
      activeRow.workerIndices.forEach(function (idx) {
        activeCohortSet[idx] = true;
      });
    }

    if (state.viewMode === "matrix") {
      var tilesHtml = vm.workers
        .map(function (w) {
          var sel = w.index === state.selectedWorkerIndex;
          return (
            '<button type="button" class="full-matrix-tile" role="option" ' +
            'aria-selected="' +
            (sel ? "true" : "false") +
            '" data-worker-index="' +
            w.index +
            '">' +
            "<span>" +
            escapeHtml(w.workerId.replace("agent-", "#")) +
            "</span>" +
            (w.durationMs == null ? "" : "<span>" + formatSecondsFromMs(w.durationMs) + "</span>") +
            "</button>"
          );
        })
        .join("");
      return '<div class="full-matrix-stage" role="listbox" aria-label="All worker sandboxes">' + tilesHtml + "</div>";
    }

    if (state.viewMode === "ledger") {
      var sampleWorkers = vm.workers;
      var showFences = vm.workers.some(function (worker) { return worker.fence != null; });
      var showKeys = !vm.isRealData || vm.keysInWorkers != null;
      var perCallMicro = vm.modelCalls > 0 && vm.settledMicro != null ? Math.round(vm.settledMicro / vm.modelCalls) : null;
      var rowsHtml = sampleWorkers
        .map(function (w) {
          var wMicro = vm.isRealData ? w.settledMicro : w.calls * perCallMicro;
          return (
            "<tr>" +
            "<td>" +
            escapeHtml(w.workerId) +
            "</td>" +
            "<td>" +
            escapeHtml(w.taskId) +
            "</td>" +
            (showFences ? "<td>" + (w.fence == null ? "—" : escapeHtml(w.fence)) + "</td>" : "") +
            "<td>" +
            formatNumber(w.calls) +
            "</td>" +
            "<td>" +
            formatUsdFromMicro(wMicro, 3) +
            "</td>" +
            (showKeys ? "<td>" + (vm.isRealData ? formatNumber(vm.keysInWorkers) : "0") + "</td>" : "") +
            "</tr>"
          );
        })
        .join("");

      return (
        '<div class="ledger-stage">' +
        (vm.heldMicro > 0 ? '<div class="ledger-held">' + formatUsdFromMicro(vm.heldMicro, 3) + ' held</div>' : '') +
        '<table class="ledger-table">' +
        "<thead><tr><th>Agent</th><th>Task</th>" + (showFences ? "<th>Fence</th>" : "") + "<th>Calls</th><th>Spend</th>" + (showKeys ? "<th>Keys</th>" : "") + "</tr></thead>" +
        "<tbody>" +
        rowsHtml +
        "</tbody>" +
        "</table>" +
        "</div>"
      );
    }

    /* Default: Execution Waterfall + Synchronized 100-Unit Micro-Matrix Strip */
    var wfRowsHtml = vm.timelineRows
      .map(function (row) {
        var isRowSelected = row.rowIndex === state.selectedRowIndex;
        var shardWidthPct = (vm.totalMs > 0 && row.durationMs != null ? Math.min(100, row.durationMs / vm.totalMs * 100) : 0).toFixed(2);
        var synthLeftPct = (vm.totalMs > 0 ? vm.parallelMs / vm.totalMs * 100 : 0).toFixed(2);
        var synthWidthPct = (vm.totalMs > 0 ? row.synthesisMs / vm.totalMs * 100 : 0).toFixed(2);
        var rightDur = row.hasSynthesis
          ? formatSecondsFromMs(vm.totalMs)
          : formatSecondsFromMs(row.durationMs);
        var rowLabel = row.workerIndices.length > 1
          ? String(row.workerIndices[0]).padStart(2, "0") + "–" + String(row.workerIndices[row.workerIndices.length - 1]).padStart(2, "0")
          : row.label;

        return (
          '<button type="button" class="wf-row" role="option" title="' + escapeHtml(row.label) + '" aria-label="' + escapeHtml(row.label) + '" style="--row-i:' +
          row.rowIndex +
          '" aria-selected="' +
          (isRowSelected ? "true" : "false") +
          '" data-row-index="' +
          row.rowIndex +
          '">' +
          '<span class="wf-agent">' +
          escapeHtml(rowLabel) +
          "</span>" +
          '<span class="wf-track">' +
          '<span class="wf-bar-shard" style="width:' +
          shardWidthPct +
          '%"></span>' +
          (row.hasSynthesis
            ? '<span class="wf-bar-synth" style="left:' +
              synthLeftPct +
              "%;width:" +
              synthWidthPct +
              '%">+' +
              formatSecondsFromMs(row.synthesisMs) +
              " synthesis</span>"
            : "") +
          "</span>" +
          '<span class="wf-dur">' +
          rightDur +
          "</span>" +
          "</button>"
        );
      })
      .join("");

    var matrixCellsHtml = vm.workers
      .map(function (w) {
        var isSelected = w.index === state.selectedWorkerIndex;
        var inCohort = Boolean(activeCohortSet[w.index]);
        var shortId = String(w.index).padStart(2, "0");
        return (
          '<button type="button" class="matrix-cell" role="option" ' +
          'aria-selected="' +
          (isSelected ? "true" : "false") +
          '" data-in-cohort="' +
          (inCohort ? "true" : "false") +
          '" data-worker-index="' +
          w.index +
          '" title="' +
          escapeHtml(w.workerId + " · " + w.taskId + " · fence " + w.fence) +
          '">' +
          (is100 ? shortId : escapeHtml(w.workerId)) +
          "</button>"
        );
      })
      .join("");

    return (
      '<div class="waterfall-body" style="--split-ratio:' +
      splitRatio +
      '">' +
      '<div class="wf-list" role="listbox" aria-label="Shard execution waterfall">' +
      '<div class="wf-guide-start"></div>' +
      '<div class="wf-guide-split"></div>' +
      '<div class="wf-guide-end"></div>' +
      wfRowsHtml +
      "</div>" +
      '<div class="wf-axis">' +
      "<span></span>" +
      '<div class="axis-ticks">' +
      "<span>0.0s</span>" +
      '<span class="axis-split-label">' +
      formatSecondsFromMs(vm.parallelMs) +
      "</span>" +
      "<span>" +
      formatSecondsFromMs(vm.totalMs) +
      "</span>" +
      "</div>" +
      "<span></span>" +
      "</div>" +
      "</div>" +
      '<div class="matrix-strip">' +
      '<div class="matrix-strip-head">' +
      '<span class="matrix-strip-title">Agents</span>' +
      "</div>" +
      '<div class="' +
      (is100 ? "matrix-grid-100" : "matrix-grid-10") +
      '" role="listbox" aria-label="Worker sandbox fleet">' +
      matrixCellsHtml +
      "</div>" +
      "</div>"
    );
  }

  var motionPaused = false;
  var spatialFormation = "orbit";
  var motionPreference = window.matchMedia("(prefers-reduced-motion: reduce)");
  var lastRender = null;

  function selectSpatialWorker(index) {
    var vm = normalizeSnapshotPacket(getActivePacket());
    if (!Number.isInteger(index) || index < 0 || index >= vm.workers.length) return;
    state.selectedWorkerIndex = index;
    state.selectedRowIndex = vm.workers.length > 10 ? Math.floor(index / Math.ceil(vm.workers.length / 10)) : index;
    render();
  }

  function syncExpandedScene() {
    var stage = document.getElementById("spatial-stage");
    var expanded = stage.classList.contains("is-expanded");
    document.body.classList.toggle("scene-expanded", expanded);
    document.querySelectorAll(".cmd-header, .metric-strip, .workspace-split").forEach(function (element) {
      element.inert = expanded;
    });
    if (expanded) {
      stage.setAttribute("role", "dialog");
      stage.setAttribute("aria-modal", "true");
    } else {
      stage.removeAttribute("role");
      stage.removeAttribute("aria-modal");
    }
    var button = document.getElementById("btn-expand");
    button.setAttribute("aria-label", expanded ? "Collapse swarm" : "Expand swarm");
    button.setAttribute("title", expanded ? "Collapse swarm" : "Expand swarm");
  }

  function render() {
    var app = document.getElementById("app");
    if (!app) return;
    if (counterAnimFrame) cancelAnimationFrame(counterAnimFrame);
    var spatialStage = document.getElementById("spatial-stage");
    var focused = document.activeElement;
    var focusSelector = focused && focused.id ? "#" + focused.id : null;
    ["data-worker-index", "data-row-index", "data-view", "data-preset", "data-formation"].forEach(function (attr) {
      if (focused && focused.hasAttribute(attr)) {
        focusSelector = "[" + attr + '="' + focused.getAttribute(attr) + '"]';
      }
    });
    if (spatialStage) spatialStage.remove();

    var vm = normalizeSnapshotPacket(getActivePacket());
    if (state.feedLoading || state.feedFailed && vm.isRealData && !vm.workers.length) {
      vm.workerCount = vm.completedShards = vm.modelCalls = null;
    }
    var hasTiming = !vm.isRealData || vm.timelineRows.some(function (row) { return row.durationMs != null; });
    if (!hasTiming && !state.feedLoading && state.viewMode === "waterfall") state.viewMode = "matrix";
    if (state.selectedWorkerIndex >= vm.workers.length) {
      state.selectedWorkerIndex = 0;
    }
    if (state.selectedRowIndex >= vm.timelineRows.length) {
      state.selectedRowIndex = 0;
    }

    var selectedWorker = vm.workers[state.selectedWorkerIndex] || vm.workers[0] || {
      workerId: "—", taskId: "—", fence: null, durationMs: null, calls: null, tokens: null,
      settledMicro: null, findingSummary: "No worker selected"
    };
    var activeRow = vm.timelineRows[state.selectedRowIndex] || vm.timelineRows[0];
    var splitRatio = (vm.totalMs > 0 ? vm.parallelMs / vm.totalMs : 0).toFixed(4);
    var settledPct = vm.ceilingMicro > 0 && vm.settledMicro != null ? Math.min(100, Math.max(vm.isRealData ? 0 : 2, (vm.settledMicro / vm.ceilingMicro) * 100)).toFixed(2) : null;
    var segIndex = vm.isRealData ? -1 : !state.customSnapshot && state.presetKey === "live-20260412-132831" ? 1 : 0;
    var stageKey = (vm.isRealData ? vm.goalId + ":" + vm.workers.length : state.presetKey) + ":" + (state.customFilename || "") + ":" + state.viewMode;
    var workspaceClass = "workspace-split" +
      (!lastRender || lastRender.stageKey !== stageKey ? " is-view-enter" : "") +
      (lastRender && lastRender.worker !== selectedWorker.workerId ? " is-selection-change" : "");
    var previousViewIndex = ["waterfall", "matrix", "ledger"].indexOf(lastRender ? lastRender.view : state.viewMode);

    var perCallTok = vm.modelCalls > 0 && vm.totalTokens != null ? Math.round(vm.totalTokens / vm.modelCalls) : null;
    var workerTok = vm.isRealData ? selectedWorker.tokens : selectedWorker.calls * perCallTok;
    var workerMicro = vm.isRealData ? selectedWorker.settledMicro : Math.round((vm.settledMicro / vm.modelCalls) * selectedWorker.calls);

    var daemonsHtml = vm.daemons
      .map(function (d) {
        return (
          '<div class="enclave-row">' +
          '<span><span class="enclave-svc">' +
          escapeHtml(d.service) +
          '</span> <span class="enclave-uid">:' +
          escapeHtml(d.uid) +
          "</span></span>" +
          '<span class="enclave-ok">' +
          escapeHtml(d.badge) +
          "</span>" +
          "</div>"
        );
      })
      .join("");

    var cmdkModalHtml = "";
    if (state.cmdkOpen) {
      var cmdkItems = buildCmdkItems(vm);
      if (state.cmdkActiveIdx >= cmdkItems.length) {
        state.cmdkActiveIdx = 0;
      }
      var listHtml = cmdkItems
        .map(function (it, idx) {
          return (
            '<button type="button" class="cmdk-item" data-cmdk-idx="' +
            idx +
            '" data-active="' +
            (idx === state.cmdkActiveIdx ? "true" : "false") +
            '">' +
            "<span>" +
            escapeHtml(it.label) +
            "</span>" +
            '<span class="cmdk-item-right">' +
            escapeHtml(it.right) +
            "</span>" +
            "</button>"
          );
        })
        .join("");

      cmdkModalHtml =
        '<div class="cmdk-backdrop" id="cmdk-backdrop" role="dialog" aria-modal="true" aria-label="Command Menu">' +
        '<div class="cmdk-dialog">' +
        '<div class="cmdk-input-wrap">' +
        '<span class="cmdk-prompt">&gt;_</span>' +
        '<input id="cmdk-input" class="cmdk-input" type="text" placeholder="Find agent or action…" value="' +
        escapeHtml(state.cmdkQuery) +
        '" autocomplete="off" />' +
        "<kbd>ESC</kbd>" +
        "</div>" +
        '<div class="cmdk-list">' +
        listHtml +
        "</div>" +
        '<div class="cmdk-footer">' +
        "<span>↑↓ ↵</span>" +
        "</div>" +
        "</div>" +
        "</div>";
    }

    app.innerHTML =
      '<main class="instrument-shell" aria-label="solos-swarm Telemetry Monitor">' +
      /* Tier 1: Slim 48px Command Header Bar */
      '<header class="cmd-header">' +
      '<div class="cmd-left">' +
      '<span class="brand-mark"><span class="beacon-dot"></span>solos-swarm</span>' +
      '<span class="crumb-sep">/</span>' +
      '<span class="goal-slug">' +
      escapeHtml(vm.goalId) +
      "</span>" +
      "</div>" +
      '<div class="cmd-right">' +
      '<div class="seg-control"' + (vm.isRealData ? " hidden" : "") + ' data-runtime="' + Boolean(vm.isRealData) + '" role="group" aria-label="Swarm Cohort Preset" style="--seg-index:' +
      segIndex +
      '">' +
      '<span class="seg-thumb"></span>' +
      '<button type="button" class="seg-btn" data-preset="swarm-100" aria-pressed="' +
      (segIndex === 0 ? "true" : "false") +
      '">100 agents</button>' +
      '<button type="button" class="seg-btn" data-preset="live-20260412-132831" aria-pressed="' +
      (segIndex === 1 ? "true" : "false") +
      '">10 agents</button>' +
      "</div>" +
      '<button type="button" class="tool-btn" id="btn-cmdk" aria-label="Open command palette">' +
      "<span>Jump</span><kbd>⌘K</kbd>" +
      "</button>" +
      '<button type="button" class="tool-btn" id="btn-load-snapshot" title="Import snapshot">' +
      "<span>" +
      "Import" +
      "</span>" +
      "</button>" +
      '<button type="button" class="tool-btn theme-toggle" id="btn-theme-toggle" aria-label="Toggle theme" title="Theme · T">' +
      '<svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" aria-hidden="true"><circle cx="8" cy="8" r="3"/><path d="M8 0v2m0 12v2M0 8h2m12 0h2M2.3 2.3l1.4 1.4m8.6 8.6 1.4 1.4M2.3 13.7l1.4-1.4m8.6-8.6 1.4-1.4"/></svg>' +
      "</button>" +
      "</div>" +
      "</header>" +
      '<div class="observatory">' +
      /* Tier 2: Unified 4-Cell Hairline Metric Strip */
      '<section class="metric-strip" aria-label="Swarm Key Performance Indicators">' +
      '<article class="metric-cell">' +
      '<div class="metric-head">' +
      '<span class="metric-label">Agents</span>' +
      "</div>" +
      '<div class="metric-val" id="metric-workers">' +
      formatNumber(vm.workerCount) +
      "</div>" +
      '<div class="metric-proof" id="metric-capacity"' + (vm.configuredWorkers == null || vm.configuredWorkers === vm.workerCount ? ' hidden' : '') + '>/ ' + formatNumber(vm.configuredWorkers) + ' configured</div>' +
      "</article>" +
      '<article class="metric-cell">' +
      '<div class="metric-head">' +
      '<span class="metric-label">Spend</span>' +
      "</div>" +
      '<div class="metric-val" id="metric-spend">' +
      formatUsdFromMicro(vm.settledMicro, 3) +
      "</div>" +
      '<div class="metric-proof" id="metric-cap">/ ' + formatUsdFromMicro(vm.ceilingMicro, 2) +
      (vm.unknownHolds > 0 ? " · " + formatNumber(vm.unknownHolds) + " unknown holds" : "") + '</div>' +
      '<div class="metric-progress-track" role="progressbar" aria-label="Spend ledger" ' +
      (settledPct == null ? 'aria-valuetext="Unknown"' : 'aria-valuenow="' + vm.settledMicro + '" aria-valuemin="0" aria-valuemax="' + vm.ceilingMicro + '"') + '>' +
      '<span class="metric-progress-fill" style="width:' +
      (settledPct || "0") +
      '%"></span>' +
      "</div>" +
      "</article>" +
      '<article class="metric-cell">' +
      '<div class="metric-head">' +
      '<span class="metric-label">Calls</span>' +
      "</div>" +
      '<div class="metric-val" id="metric-calls">' +
      formatNumber(vm.modelCalls) +
      "</div>" +
      "</article>" +
      '<article class="metric-cell">' +
      '<div class="metric-head">' +
      '<span class="metric-label">Elapsed</span>' +
      "</div>" +
      '<div class="metric-val">' +
      formatSecondsFromMs(vm.totalMs) +
      "</div>" +
      "</article>" +
      "</section>" +
      '<section id="spatial-stage" class="spatial-stage" aria-label="Sandbox fleet visualization">' +
      '<div class="spatial-heading"><span>Swarm <span class="source-badge" id="source-badge">Demo</span></span><span class="spatial-config"></span><button type="button" class="source-connect" id="btn-feed" aria-label="Follow runtime">Connect</button></div>' +
      '<div class="spatial-controls">' +
      '<div class="formation-controls" role="group" aria-label="Swarm formation">' +
      '<button type="button" class="formation-btn" data-formation="orbit" aria-pressed="true"><svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="3"/><ellipse cx="8" cy="8" rx="7" ry="3" transform="rotate(-35 8 8)"/></svg>Orbit</button>' +
      '<button type="button" class="formation-btn" data-formation="lattice" aria-pressed="false"><svg viewBox="0 0 16 16" aria-hidden="true"><path d="m8 1 6 3.5v7L8 15l-6-3.5v-7Zm0 7v7M2 4.5 8 8l6-3.5"/></svg>Lattice</button>' +
      '<button type="button" class="formation-btn" data-formation="helix" aria-pressed="false"><svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 1c0 5 8 2 8 7s-8 2-8 7M12 1c0 5-8 2-8 7s8 2 8 7M5 3h6M5 13h6M4 8h8"/></svg>Helix</button>' +
      "</div>" +
      '<button type="button" class="motion-toggle" id="btn-motion" aria-label="Pause motion" title="Pause motion"></button>' +
      '<button type="button" class="scene-expand" id="btn-expand" aria-label="Expand swarm" title="Expand swarm"><svg viewBox="0 0 16 16" aria-hidden="true"><path d="M6 2H2v4m8-4h4v4M2 10v4h4m8-4v4h-4"/></svg></button>' +
      "</div>" +
      '<div class="spatial-axis" aria-hidden="true"><svg viewBox="0 0 52 52"><path d="M26 26V6m0 20 18 11M26 26 8 37"/><circle cx="26" cy="26" r="2"/><text x="23" y="5">Y</text><text x="45" y="43">X</text><text x="2" y="43">Z</text></svg></div>' +
      '<span class="spatial-corner spatial-corner-tl" aria-hidden="true"></span>' +
      '<span class="spatial-corner spatial-corner-br" aria-hidden="true"></span>' +
      "</section>" +
      "</div>" +
      /* Tier 3: 72% / 28% Primary Workspace + Progressive-Disclosure Inspector */
      '<section class="' + workspaceClass + '" data-mode="' + state.viewMode + '" style="--previous-view-index:' + previousViewIndex + '" aria-label="Execution Trace and Live Unit Inspector">' +
      '<div class="canvas-pane">' +
      '<div class="pane-bar">' +
      '<div class="pane-tabs" role="tablist" aria-label="Telemetry View Mode">' +
      '<button type="button" class="pane-tab" role="tab" data-view="waterfall"' + (hasTiming ? '' : ' disabled') + ' aria-selected="' +
      (state.viewMode === "waterfall" ? "true" : "false") +
      '" title="' + (hasTiming ? 'Trace · 1' : 'Timing unavailable') + '">Trace</button>' +
      '<button type="button" class="pane-tab" role="tab" data-view="matrix" aria-selected="' +
      (state.viewMode === "matrix" ? "true" : "false") +
      '" title="Agents · 2">Agents</button>' +
      '<button type="button" class="pane-tab" role="tab" data-view="ledger" aria-selected="' +
      (state.viewMode === "ledger" ? "true" : "false") +
      '" title="Ledger · 3">Ledger</button>' +
      "</div>" +
      '<div class="pane-legend"' + (vm.isRealData || state.viewMode !== 'waterfall' ? ' hidden' : '') + '>' +
      '<span class="legend-item"><span class="swatch shard"></span>Shards</span>' +
      '<span class="legend-item"><span class="swatch synth"></span>Synthesis</span>' +
      "</div>" +
      "</div>" +
      renderMainStage(vm, splitRatio, activeRow) +
      "</div>" +
      /* Right Column: Progressive-Disclosure Live Inspector */
      '<aside class="inspector-pane" aria-label="Selected Worker Telemetry">' +
      '<div class="inspector-animated">' +
      '<div class="inspector-head">' +
      '<span class="inspector-title">Agent</span>' +
      '<span class="inspector-worker-pill">' +
      escapeHtml(selectedWorker.workerId) +
      "</span>" +
      "</div>" +
      '<div class="kv-table">' +
      '<div class="kv-row"><span class="kv-key">Task</span><span class="kv-val">' +
      escapeHtml(selectedWorker.taskId) +
      "</span></div>" +
      '<div class="kv-row"' + (selectedWorker.fence == null ? ' hidden' : '') + '><span class="kv-key">Fence</span><span class="kv-val">' +
      formatNumber(selectedWorker.fence) + '</span></div>' +
      '<div class="kv-row"' + (selectedWorker.durationMs == null ? ' hidden' : '') + '><span class="kv-key">Time</span><span class="kv-val">' +
      formatSecondsFromMs(selectedWorker.durationMs) +
      "</span></div>" +
      '<div class="kv-row"><span class="kv-key">Usage</span><span class="kv-val">' +
      formatNumber(selectedWorker.calls) +
      " calls · " +
      formatTokens(workerTok) +
      " tok · " +
      formatUsdFromMicro(workerMicro, 3) + '</span></div>' +
      '<div class="kv-row"' + (vm.isRealData && vm.keysInWorkers == null ? ' hidden' : '') + '><span class="kv-key">Keys</span><span class="kv-val">' + (vm.isRealData ? formatNumber(vm.keysInWorkers) : "0") + '</span></div>' +
      "</div>" +
      '<div class="finding-section"' + (!vm.isRealData || selectedWorker.findingSummary === 'No finding recorded' || selectedWorker.findingSummary === 'No worker selected' ? ' hidden' : '') + '>' +
      '<span class="finding-label">Finding</span>' +
      '<div class="finding-box">' +
      escapeHtml(selectedWorker.findingSummary === 'Finding recorded' ? 'Filed' : selectedWorker.findingSummary) +
      "</div>" +
      "</div>" +
      "</div>" +
      '<details class="enclave-block"' + (vm.isRealData && !vm.daemons.length ? " hidden" : "") + '>' +
      '<summary class="enclave-header">Isolation</summary>' +
      daemonsHtml +
      "</details>" +
      "</aside>" +
      "</section>" +
      "</main>" +
      cmdkModalHtml;

    var nextStage = document.getElementById("spatial-stage");
    if (spatialStage) nextStage.replaceWith(spatialStage);
    else {
      spatialStage = nextStage;
      if (window.SwarmSpatial) window.SwarmSpatial.mount(spatialStage);
    }
    var source = vm.source || {};
    var sourceAge = source.updated_at == null ? null : Math.max(0, Date.now() - source.updated_at);
    var sourceStatus = state.feedLoading ? "Connecting" : vm.isRealData
      ? (state.feedFailed || source.connected === false && state.feedEnabled ? (vm.workers.length ? "Stale" : "Unavailable")
        : source.service_active === true && sourceAge != null && sourceAge < 30000 ? "Live"
        : vm.statusLabel === "RUNNING" && sourceAge != null && sourceAge >= 30000 ? "Stale" : "Snapshot")
      : "Demo";
    var sourceBadge = spatialStage.querySelector("#source-badge");
    sourceBadge.textContent = sourceStatus;
    sourceBadge.setAttribute("data-status", sourceStatus.toLowerCase());
    sourceBadge.title = (source.label || (sourceStatus === "Demo" ? "Bundled example snapshot" : "Swarm source")) +
      (source.updated_at == null ? "" : " · " + new Date(source.updated_at).toLocaleString());
    spatialStage.querySelector(".spatial-config").textContent = vm.model || vm.provider || "";
    spatialStage.querySelector("#btn-feed").hidden = state.feedEnabled && Boolean(state.feedSnapshot) && !state.feedFailed;
    spatialStage.setAttribute("data-formation-state", spatialFormation);
    spatialStage.querySelectorAll("[data-formation]").forEach(function (button) {
      button.setAttribute("aria-pressed", String(button.getAttribute("data-formation") === spatialFormation));
    });
    var motionButton = spatialStage.querySelector("#btn-motion");
    motionButton.setAttribute("aria-label", motionPaused ? "Resume motion" : "Pause motion");
    motionButton.setAttribute("title", motionPaused ? "Resume motion" : "Pause motion");
    motionButton.setAttribute("aria-pressed", String(motionPaused));
    motionButton.disabled = motionPreference.matches;
    if (motionPreference.matches) {
      motionButton.setAttribute("aria-label", "Motion reduced by system preference");
      motionButton.setAttribute("title", "Motion reduced by system preference");
    }
    motionButton.innerHTML = '<svg width="14" height="14" viewBox="0 0 14 14" fill="none" aria-hidden="true">' +
      (motionPaused ? '<path d="M4 2.5 11 7l-7 4.5z" fill="currentColor"/>' : '<path d="M4.5 3v8m5-8v8" stroke="currentColor" stroke-width="1.5"/>') + '</svg>';
    if (window.SwarmSpatial) window.SwarmSpatial.update({
      workers: vm.workers,
      selectedWorkerIndex: state.selectedWorkerIndex,
      selectedRowIndex: state.selectedRowIndex,
      cohortIndices: activeRow ? activeRow.workerIndices : [],
      formation: spatialFormation,
      onSelect: selectSpatialWorker,
      activityEnabled: sourceStatus === "Live" || sourceStatus === "Demo",
      paused: motionPaused
    });
    syncExpandedScene();

    var reducedMotion = motionPreference.matches;
    if (!reducedMotion && !lastRender) {
      app.animate([{ opacity: 0, transform: "perspective(1200px) translateY(20px) rotateX(2deg)" }, { opacity: 1, transform: "none" }],
        { duration: 850, easing: "cubic-bezier(.16,1,.3,1)" });
    }
    if (!reducedMotion && (!lastRender || lastRender.stageKey !== stageKey)) {
      app.querySelectorAll(".wf-bar-shard, .wf-bar-synth, .full-matrix-tile").forEach(function (el, index) {
        el.animate([{ opacity: 0, transform: "translateY(8px) scaleX(.88)" }, { opacity: 1, transform: "none" }],
          { duration: 600, delay: Math.min(index * 18, 350), easing: "cubic-bezier(.16,1,.3,1)", fill: "backwards" });
      });
      app.querySelector(".canvas-pane").animate([{ opacity: 0.5, transform: "translateY(8px)" }, { opacity: 1, transform: "none" }],
        { duration: 450, easing: "cubic-bezier(.16,1,.3,1)" });
    }
    if (!reducedMotion && lastRender && lastRender.worker !== selectedWorker.workerId) {
      app.querySelector(".inspector-animated").animate([{ opacity: 0.3, transform: "translateX(10px)" }, { opacity: 1, transform: "none" }],
        { duration: 320, easing: "cubic-bezier(.16,1,.3,1)" });
    }
    lastRender = { stageKey: stageKey, worker: selectedWorker.workerId, view: state.viewMode };
    if (focusSelector && !state.cmdkOpen) {
      var focusTarget = app.querySelector(focusSelector);
      if (focusTarget) focusTarget.focus({ preventScroll: true });
    }

    if (state.cmdkOpen) {
      var inputEl = document.getElementById("cmdk-input");
      if (inputEl) {
        inputEl.focus();
        var len = inputEl.value.length;
        inputEl.setSelectionRange(len, len);
      }
    }
  }

  function switchPreset(nextKey) {
    if (state.presetKey === nextKey && !state.customSnapshot && !state.feedSnapshot) return;
    var prevVm = normalizeSnapshotPacket(getActivePacket());
    state.feedEnabled = false;
    state.feedSnapshot = null;
    state.feedFailed = false;
    state.feedLoading = false;
    state.presetKey = nextKey;
    state.customSnapshot = null;
    state.customFilename = null;
    state.selectedRowIndex = 0;
    state.selectedWorkerIndex = 0;
    var nextVm = normalizeSnapshotPacket(getActivePacket());
    render();
    animateMetricCounters(prevVm, nextVm);
  }

  function loadExternalSnapshots() {
    ["swarm-100", "live-20260412-132831"].forEach(function (key) {
      fetch("snapshots/" + key + ".json")
        .then(function (res) {
          if (!res.ok) throw new Error("HTTP " + res.status);
          return res.json();
        })
        .then(function (data) {
          state.snapshots[key] = data;
          if (!state.customSnapshot && !state.feedSnapshot && state.presetKey === key) {
            render();
          }
        })
        .catch(function () {
          /* Embedded fallback preset active */
        });
    });
  }

  var feedTimer = null;
  var feedInFlight = false;
  var feedFingerprint = "";
  function followRuntime() {
    state.feedEnabled = true;
    state.customSnapshot = null;
    state.customFilename = null;
    state.feedLoading = true;
    if (!state.feedSnapshot) state.feedSnapshot = { schema: "solos-swarm.telemetry/v1", source: { label: "Connecting", connected: false }, config: {}, state: {} };
    feedFingerprint = "";
    render();
    loadRuntimeFeed(false);
  }

  async function loadRuntimeFeed(initial) {
    clearTimeout(feedTimer);
    if (!state.feedEnabled || feedInFlight) return;
    feedInFlight = true;
    var changed = false;
    try {
      var response = await fetch("/api/swarm", { cache: "no-store", signal: AbortSignal.timeout(12000) });
      if (!state.feedEnabled) return;
      if (response.status === 404 && initial) {
        state.feedEnabled = false;
        state.feedSnapshot = null;
      } else {
        if (!response.ok) throw new Error("Runtime source unavailable");
        var packet = await response.json();
        if (packet.schema !== "solos-swarm.telemetry/v1") throw new Error("Invalid telemetry source");
        var packetAge = packet.source && packet.source.updated_at != null ? Date.now() - packet.source.updated_at : null;
        var fingerprint = JSON.stringify(packet) + ":fresh=" + (packetAge != null && packetAge < 30000);
        changed = state.feedFailed || fingerprint !== feedFingerprint;
        if (state.feedSnapshot && state.feedSnapshot.source && packet.source && state.feedSnapshot.source.label !== packet.source.label) {
          state.selectedRowIndex = state.selectedWorkerIndex = 0;
        }
        feedFingerprint = fingerprint;
        state.feedSnapshot = packet;
        state.feedFailed = false;
      }
    } catch (_) {
      if (!state.feedEnabled) return;
      changed = !state.feedFailed;
      state.feedFailed = true;
      if (!state.feedSnapshot) {
        state.feedSnapshot = { schema: "solos-swarm.telemetry/v1", source: { label: "Runtime unavailable", connected: false }, config: {}, state: {} };
      }
    } finally {
      feedInFlight = false;
      state.feedLoading = false;
      if (initial || changed) render();
      if (initial) loadExternalSnapshots();
      if (state.feedEnabled) {
        feedTimer = setTimeout(function poll() {
          if (!state.feedEnabled) return;
          if (document.hidden) {
            feedTimer = setTimeout(poll, 5000);
            return;
          }
          loadRuntimeFeed(false);
        }, 5000);
      }
    }
  }

  function handleFileImport(file) {
    if (!file) return;
    var reader = new FileReader();
    reader.onload = function (ev) {
      try {
        var parsed = JSON.parse(String(ev.target.result || "{}"));
        state.customSnapshot = parsed;
        state.feedEnabled = false;
        state.feedSnapshot = null;
        state.feedFailed = false;
        state.feedLoading = false;
        state.customFilename = file.name;
        state.selectedRowIndex = 0;
        state.selectedWorkerIndex = 0;
        render();
      } catch (_) {
        /* ignore invalid JSON */
      }
    };
    reader.readAsText(file);
  }

  function bindEvents() {
    motionPreference.addEventListener("change", function () {
      if (motionPreference.matches) {
        document.getAnimations().forEach(function (animation) { animation.cancel(); });
      }
      render();
    });
    document.addEventListener("click", function (ev) {
      if (ev.target.closest("#btn-feed")) {
        followRuntime();
        return;
      }
      if (ev.target.closest("#btn-expand")) {
        var stage = document.getElementById("spatial-stage");
        stage.classList.toggle("is-expanded");
        syncExpandedScene();
        return;
      }
      var formationButton = ev.target.closest("button[data-formation]");
      if (formationButton) {
        spatialFormation = formationButton.getAttribute("data-formation");
        render();
        return;
      }
      if (ev.target.closest("#btn-motion")) {
        motionPaused = !motionPaused;
        render();
        return;
      }
      var presetBtn = ev.target.closest("[data-preset]");
      if (presetBtn) {
        switchPreset(presetBtn.getAttribute("data-preset"));
        return;
      }

      var viewBtn = ev.target.closest("[data-view]");
      if (viewBtn) {
        state.viewMode = viewBtn.getAttribute("data-view");
        render();
        return;
      }

      var cmdkBtn = ev.target.closest("#btn-cmdk");
      if (cmdkBtn) {
        state.cmdkOpen = true;
        state.cmdkQuery = "";
        state.cmdkActiveIdx = 0;
        render();
        return;
      }

      if (ev.target && ev.target.id === "cmdk-backdrop") {
        state.cmdkOpen = false;
        render();
        return;
      }

      var cmdkItemBtn = ev.target.closest("[data-cmdk-idx]");
      if (cmdkItemBtn) {
        var idx = Number(cmdkItemBtn.getAttribute("data-cmdk-idx")) || 0;
        var vm = normalizeSnapshotPacket(getActivePacket());
        var items = buildCmdkItems(vm);
        if (items[idx]) {
          state.cmdkOpen = false;
          items[idx].action();
          render();
        }
        return;
      }

      var loadBtn = ev.target.closest("#btn-load-snapshot");
      if (loadBtn) {
        var input = document.getElementById("snapshot-file-input");
        if (input) input.click();
        return;
      }

      var themeBtn = ev.target.closest("#btn-theme-toggle");
      if (themeBtn) {
        toggleTheme();
        return;
      }

      var rowBtn = ev.target.closest("[data-row-index]");
      if (rowBtn) {
        var rIdx = Number(rowBtn.getAttribute("data-row-index")) || 0;
        var vm2 = normalizeSnapshotPacket(getActivePacket());
        var row = vm2.timelineRows[rIdx];
        state.selectedRowIndex = rIdx;
        if (row && row.workerIndices && row.workerIndices.length > 0) {
          state.selectedWorkerIndex = row.workerIndices[0];
        }
        render();
        return;
      }

      var workerBtn = ev.target.closest("[data-worker-index]");
      if (workerBtn) {
        var wIdx = Number(workerBtn.getAttribute("data-worker-index")) || 0;
        var vm3 = normalizeSnapshotPacket(getActivePacket());
        state.selectedWorkerIndex = wIdx;
        state.selectedRowIndex = vm3.workers.length > 10 ? Math.floor(wIdx / Math.ceil(vm3.workers.length / 10)) : wIdx;
        render();
      }
    });

    document.addEventListener("input", function (ev) {
      if (ev.target && ev.target.id === "cmdk-input") {
        state.cmdkQuery = ev.target.value;
        state.cmdkActiveIdx = 0;
        render();
      }
    });

    var fileInput = document.getElementById("snapshot-file-input");
    if (fileInput) {
      fileInput.addEventListener("change", function (ev) {
        var file = ev.target.files && ev.target.files[0];
        if (file) handleFileImport(file);
      });
    }

    document.addEventListener("keydown", function (ev) {
      if (ev.key === "Escape" && document.body.classList.contains("scene-expanded") && !state.cmdkOpen) {
        document.getElementById("btn-expand").click();
        document.getElementById("btn-expand").focus();
        return;
      }
      if (ev.key === "Tab" && document.body.classList.contains("scene-expanded") && !state.cmdkOpen) {
        var controls = Array.from(document.querySelectorAll("#spatial-stage button:not(:disabled)")).filter(function (button) {
          return button.getClientRects().length > 0;
        });
        var current = controls.indexOf(document.activeElement);
        if (current < 0 || (ev.shiftKey && current === 0) || (!ev.shiftKey && current === controls.length - 1)) {
          ev.preventDefault();
          controls[ev.shiftKey ? controls.length - 1 : 0].focus();
        }
      }
      if ((ev.metaKey || ev.ctrlKey) && (ev.key === "k" || ev.key === "K")) {
        ev.preventDefault();
        state.cmdkOpen = !state.cmdkOpen;
        state.cmdkQuery = "";
        state.cmdkActiveIdx = 0;
        render();
        return;
      }

      if (state.cmdkOpen) {
        var vmCmd = normalizeSnapshotPacket(getActivePacket());
        var items = buildCmdkItems(vmCmd);
        if (ev.key === "Escape") {
          ev.preventDefault();
          state.cmdkOpen = false;
          render();
        } else if (ev.key === "ArrowDown") {
          ev.preventDefault();
          state.cmdkActiveIdx = (state.cmdkActiveIdx + 1) % Math.max(1, items.length);
          render();
        } else if (ev.key === "ArrowUp") {
          ev.preventDefault();
          state.cmdkActiveIdx =
            (state.cmdkActiveIdx - 1 + Math.max(1, items.length)) % Math.max(1, items.length);
          render();
        } else if (ev.key === "Enter" && items[state.cmdkActiveIdx]) {
          ev.preventDefault();
          state.cmdkOpen = false;
          items[state.cmdkActiveIdx].action();
          render();
        }
        return;
      }

      if (ev.target && (ev.target.tagName === "INPUT" || ev.target.tagName === "TEXTAREA")) {
        return;
      }

      if (ev.key === "/") {
        ev.preventDefault();
        state.cmdkOpen = true;
        state.cmdkQuery = "";
        state.cmdkActiveIdx = 0;
        render();
        return;
      }

      if ((ev.key === "t" || ev.key === "T") && !ev.metaKey && !ev.ctrlKey && !ev.altKey) {
        ev.preventDefault();
        toggleTheme();
        return;
      }

      if (ev.key === "1") {
        state.viewMode = "waterfall";
        render();
        return;
      }
      if (ev.key === "2") {
        state.viewMode = "matrix";
        render();
        return;
      }
      if (ev.key === "3") {
        state.viewMode = "ledger";
        render();
        return;
      }

      if (ev.key === "ArrowRight" || ev.key === "ArrowLeft" || ev.key === "ArrowDown" || ev.key === "ArrowUp") {
        var vm = normalizeSnapshotPacket(getActivePacket());
        var total = vm.workers.length;
        if (!total) return;
        ev.preventDefault();
        var workerControl = ev.target.closest("[data-worker-index]");
        var rowControl = ev.target.closest("[data-row-index]");
        var step = 1;
        if (ev.key === "ArrowDown" || ev.key === "ArrowUp") {
          step = total > 10 ? Math.ceil(total / 10) : 1;
          if (workerControl) {
            step = getComputedStyle(workerControl.parentElement).gridTemplateColumns.split(" ").length;
          }
        }
        var dir = ev.key === "ArrowRight" || ev.key === "ArrowDown" ? 1 : -1;
        var next = (state.selectedWorkerIndex + dir * step + total) % total;
        state.selectedWorkerIndex = next;
        state.selectedRowIndex = total > 10 ? Math.floor(next / Math.ceil(total / 10)) : next;
        render();
        var nextFocus = workerControl
          ? document.querySelector('[data-worker-index="' + next + '"]')
          : rowControl && document.querySelector('[data-row-index="' + state.selectedRowIndex + '"]');
        if (nextFocus) nextFocus.focus({ preventScroll: true });
      }
    });
  }

  parseInitialUrlParams();
  bindEvents();
  if (state.feedEnabled) {
    state.feedLoading = true;
    state.feedSnapshot = { schema: "solos-swarm.telemetry/v1", source: { label: "Connecting", connected: false }, config: {}, state: {} };
    render();
    loadRuntimeFeed(true);
  }
  else {
    render();
    loadExternalSnapshots();
  }
})();
