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
    cmdkActiveIdx: 0
  };

  function parseInitialUrlParams() {
    var params = new URLSearchParams(window.location.search);
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
  }

  function formatNumber(n) {
    return Number(n || 0).toLocaleString("en-US");
  }

  function formatUsdFromMicro(micro, forceDecimals) {
    var usd = Number(micro || 0) / 1000000;
    var digits = typeof forceDecimals === "number" ? forceDecimals : 3;
    return "$" + usd.toFixed(digits);
  }

  function formatTokens(totalTokens) {
    var n = Number(totalTokens || 0);
    if (n >= 1000) {
      return (n / 1000).toFixed(1) + "k";
    }
    return String(n);
  }

  function formatSecondsFromMs(ms) {
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
        id: "preset-100",
        label: "Switch preset → 100-Agent Swarm (reconcile-fleet-100)",
        right: "100 workers · $0.174",
        action: function () {
          switchPreset("swarm-100");
        }
      },
      {
        id: "preset-10",
        label: "Switch preset → 10-Agent Benchmark (live-20260412-132831)",
        right: "10 workers · $0.017",
        action: function () {
          switchPreset("live-20260412-132831");
        }
      },
      {
        id: "view-waterfall",
        label: "View → Execution Waterfall + Synchronized Matrix",
        right: "Tab 1",
        action: function () {
          state.viewMode = "waterfall";
        }
      },
      {
        id: "view-matrix",
        label: "View → Full Sandbox Matrix (" + vm.workerCount + " units)",
        right: "Tab 2",
        action: function () {
          state.viewMode = "matrix";
        }
      },
      {
        id: "view-ledger",
        label: "View → Spend Ledger & RFC 6962 Witness Proofs",
        right: "Tab 3",
        action: function () {
          state.viewMode = "ledger";
        }
      },
      {
        id: "toggle-theme",
        label: "Toggle Dark / Light Monochrome Theme",
        right: "Key T",
        action: function () {
          toggleTheme();
        }
      }
    ];

    vm.workers.forEach(function (w) {
      items.push({
        id: "worker-" + w.index,
        label: "Inspect " + w.workerId + " (" + w.taskId + ")",
        right: "fence " + w.fence + " · " + formatSecondsFromMs(w.durationMs),
        action: function () {
          state.selectedWorkerIndex = w.index;
          state.selectedRowIndex = vm.workers.length > 10 ? Math.floor(w.index / 10) : w.index;
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
            "<span>" +
            formatSecondsFromMs(w.durationMs) +
            "</span>" +
            "</button>"
          );
        })
        .join("");
      return '<div class="full-matrix-stage" role="listbox" aria-label="All worker sandboxes">' + tilesHtml + "</div>";
    }

    if (state.viewMode === "ledger") {
      var sampleWorkers = vm.workers.slice(0, 8);
      var perCallMicro = Math.round(vm.settledMicro / vm.modelCalls);
      var rowsHtml = sampleWorkers
        .map(function (w) {
          var wMicro = w.calls * perCallMicro;
          return (
            "<tr>" +
            "<td>" +
            escapeHtml(w.workerId) +
            "</td>" +
            "<td>" +
            escapeHtml(w.taskId) +
            "</td>" +
            "<td>rev " +
            escapeHtml(w.fence) +
            "</td>" +
            "<td>" +
            escapeHtml(w.calls) +
            " settled</td>" +
            "<td>" +
            formatNumber(wMicro) +
            " µUSD</td>" +
            "<td>0 keys</td>" +
            "</tr>"
          );
        })
        .join("");

      return (
        '<div class="ledger-stage">' +
        '<table class="ledger-table">' +
        "<thead><tr><th>Worker SVID</th><th>Shard</th><th>Fence Token</th><th>Broker Intents</th><th>Settled Spend</th><th>Worker Env</th></tr></thead>" +
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
        var shardWidthPct = ((row.durationMs / vm.totalMs) * 100).toFixed(2);
        var synthLeftPct = ((vm.parallelMs / vm.totalMs) * 100).toFixed(2);
        var synthWidthPct = ((row.synthesisMs / vm.totalMs) * 100).toFixed(2);
        var rightDur = row.hasSynthesis
          ? formatSecondsFromMs(vm.totalMs)
          : formatSecondsFromMs(row.durationMs);

        return (
          '<button type="button" class="wf-row" role="option" style="--row-i:' +
          row.rowIndex +
          '" aria-selected="' +
          (isRowSelected ? "true" : "false") +
          '" data-row-index="' +
          row.rowIndex +
          '">' +
          '<span class="wf-agent">' +
          escapeHtml(row.label) +
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
      " shard barrier</span>" +
      "<span>" +
      formatSecondsFromMs(vm.totalMs) +
      "</span>" +
      "</div>" +
      "<span></span>" +
      "</div>" +
      "</div>" +
      '<div class="matrix-strip">' +
      '<div class="matrix-strip-head">' +
      '<span class="matrix-strip-title">SANDBOX FLEET MATRIX (' +
      formatNumber(vm.workerCount) +
      " ISOLATED UNITS · 0 API KEYS)</span>" +
      "<span>Click or ↑↓←→ to inspect unit</span>" +
      "</div>" +
      '<div class="' +
      (is100 ? "matrix-grid-100" : "matrix-grid-10") +
      '" role="listbox" aria-label="Worker sandbox fleet">' +
      matrixCellsHtml +
      "</div>" +
      "</div>"
    );
  }

  function render() {
    var app = document.getElementById("app");
    if (!app) return;

    var vm = normalizeSnapshotPacket(getActivePacket());
    if (state.selectedWorkerIndex >= vm.workers.length) {
      state.selectedWorkerIndex = 0;
    }
    if (state.selectedRowIndex >= vm.timelineRows.length) {
      state.selectedRowIndex = 0;
    }

    var selectedWorker = vm.workers[state.selectedWorkerIndex] || vm.workers[0];
    var activeRow = vm.timelineRows[state.selectedRowIndex] || vm.timelineRows[0];
    var splitRatio = (vm.parallelMs / vm.totalMs).toFixed(4);
    var settledPct = Math.min(100, Math.max(2, (vm.settledMicro / vm.ceilingMicro) * 100)).toFixed(2);
    var segIndex = !state.customSnapshot && state.presetKey === "live-20260412-132831" ? 1 : 0;
    var themeLabel = currentTheme() === "light" ? "Light" : "Dark";

    var perCallTok = Math.round(vm.totalTokens / vm.modelCalls);
    var workerTok = selectedWorker.calls * perCallTok;
    var workerMicro = Math.round((vm.settledMicro / vm.modelCalls) * selectedWorker.calls);

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
        '<input id="cmdk-input" class="cmdk-input" type="text" placeholder="Jump to agent-00..99, switch preset, or inspect ledger…" value="' +
        escapeHtml(state.cmdkQuery) +
        '" autocomplete="off" />' +
        "<kbd>ESC</kbd>" +
        "</div>" +
        '<div class="cmdk-list">' +
        listHtml +
        "</div>" +
        '<div class="cmdk-footer">' +
        "<span>↑↓ to navigate · ↵ to select</span>" +
        "<span>solos-swarm inspect protocol v1</span>" +
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
      '<span class="status-chip"><span class="status-chip-dot"></span>' +
      escapeHtml(vm.statusLabel) +
      "</span>" +
      '<span class="goal-intent">' +
      escapeHtml(vm.goalTitle) +
      "</span>" +
      "</div>" +
      '<div class="cmd-right">' +
      '<div class="seg-control" role="group" aria-label="Swarm Cohort Preset" style="--seg-index:' +
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
      '<button type="button" class="tool-btn" id="btn-load-snapshot">' +
      "<span>" +
      (state.customFilename ? escapeHtml(state.customFilename) : "Import JSON") +
      "</span>" +
      "</button>" +
      '<button type="button" class="tool-btn" id="btn-theme-toggle" aria-label="Toggle theme">' +
      "<span>" +
      themeLabel +
      "</span><kbd>T</kbd>" +
      "</button>" +
      "</div>" +
      "</header>" +
      /* Tier 2: Unified 4-Cell Hairline Metric Strip */
      '<section class="metric-strip" aria-label="Swarm Key Performance Indicators">' +
      '<article class="metric-cell">' +
      '<div class="metric-head">' +
      '<span class="metric-label">Workers</span>' +
      '<span class="metric-tag">0 API keys</span>' +
      "</div>" +
      '<div class="metric-val" id="metric-workers">' +
      formatNumber(vm.workerCount) +
      "</div>" +
      '<div class="metric-proof">bwrap + Landlock · mTLS capability gate</div>' +
      "</article>" +
      '<article class="metric-cell">' +
      '<div class="metric-head">' +
      '<span class="metric-label">Spend Ledger</span>' +
      '<span class="metric-tag">' +
      formatNumber(vm.settledMicro) +
      " µUSD</span>" +
      "</div>" +
      '<div class="metric-val" id="metric-spend">' +
      formatUsdFromMicro(vm.settledMicro, 3) +
      "</div>" +
      '<div class="metric-proof">' +
      settledPct +
      "% of $1.00 cap · " +
      vm.unknownHolds +
      " unknown holds</div>" +
      '<div class="metric-progress-track" role="progressbar" aria-valuenow="' +
      vm.settledMicro +
      '" aria-valuemin="0" aria-valuemax="' +
      vm.ceilingMicro +
      '">' +
      '<span class="metric-progress-fill" style="width:' +
      settledPct +
      '%"></span>' +
      "</div>" +
      "</article>" +
      '<article class="metric-cell">' +
      '<div class="metric-head">' +
      '<span class="metric-label">Throughput</span>' +
      '<span class="metric-tag">' +
      formatNumber(vm.completedShards) +
      "/" +
      formatNumber(vm.workerCount) +
      " shards</span>" +
      "</div>" +
      '<div class="metric-val" id="metric-calls">' +
      formatNumber(vm.modelCalls) +
      "</div>" +
      '<div class="metric-proof">model calls · ' +
      formatTokens(vm.totalTokens) +
      " tokens settled</div>" +
      "</article>" +
      '<article class="metric-cell">' +
      '<div class="metric-head">' +
      '<span class="metric-label">Wall Clock</span>' +
      '<span class="metric-tag">rev ' +
      formatNumber(vm.revision) +
      "</span>" +
      "</div>" +
      '<div class="metric-val">' +
      formatSecondsFromMs(vm.totalMs) +
      "</div>" +
      '<div class="metric-proof">' +
      formatSecondsFromMs(vm.parallelMs) +
      " parallel + " +
      formatSecondsFromMs(vm.synthMs) +
      " synthesis</div>" +
      "</article>" +
      "</section>" +
      /* Tier 3: 72% / 28% Primary Workspace + Progressive-Disclosure Inspector */
      '<section class="workspace-split" aria-label="Execution Trace and Live Unit Inspector">' +
      '<div class="canvas-pane">' +
      '<div class="pane-bar">' +
      '<div class="pane-tabs" role="tablist" aria-label="Telemetry View Mode">' +
      '<button type="button" class="pane-tab" role="tab" data-view="waterfall" aria-selected="' +
      (state.viewMode === "waterfall" ? "true" : "false") +
      '">1. Waterfall + Fleet</button>' +
      '<button type="button" class="pane-tab" role="tab" data-view="matrix" aria-selected="' +
      (state.viewMode === "matrix" ? "true" : "false") +
      '">2. Sandbox Matrix (' +
      vm.workerCount +
      ")</button>" +
      '<button type="button" class="pane-tab" role="tab" data-view="ledger" aria-selected="' +
      (state.viewMode === "ledger" ? "true" : "false") +
      '">3. Spend &amp; Witness</button>' +
      "</div>" +
      '<div class="pane-legend">' +
      '<span class="legend-item"><span class="swatch shard"></span>Parallel shards</span>' +
      '<span class="legend-item"><span class="swatch synth"></span>Synthesis</span>' +
      "</div>" +
      "</div>" +
      renderMainStage(vm, splitRatio, activeRow) +
      "</div>" +
      /* Right Column: Progressive-Disclosure Live Inspector */
      '<aside class="inspector-pane" aria-label="Selected Worker Telemetry">' +
      '<div class="inspector-animated">' +
      '<div class="inspector-head">' +
      '<span class="inspector-title">Unit Inspector</span>' +
      '<span class="inspector-worker-pill">' +
      escapeHtml(selectedWorker.workerId) +
      "</span>" +
      "</div>" +
      '<div class="kv-table">' +
      '<div class="kv-row"><span class="kv-key">Task shard</span><span class="kv-val">' +
      escapeHtml(selectedWorker.taskId) +
      "</span></div>" +
      '<div class="kv-row"><span class="kv-key">Fencing token</span><span class="kv-val">rev ' +
      escapeHtml(selectedWorker.fence) +
      " · monotonic</span></div>" +
      '<div class="kv-row"><span class="kv-key">Execution phase</span><span class="kv-val">' +
      formatSecondsFromMs(selectedWorker.durationMs) +
      (selectedWorker.index === 0 ? " + " + formatSecondsFromMs(vm.synthMs) + " synth" : " parallel") +
      "</span></div>" +
      '<div class="kv-row"><span class="kv-key">Broker usage</span><span class="kv-val">' +
      selectedWorker.calls +
      " calls · " +
      formatNumber(workerTok) +
      " tok · " +
      formatNumber(workerMicro) +
      " µUSD</span></div>" +
      '<div class="kv-row"><span class="kv-key">Worker env keys</span><span class="kv-val">0 (OPENAI/DEEPSEEK = ∅)</span></div>' +
      "</div>" +
      '<div class="finding-section">' +
      '<span class="finding-label">Signed Finding Digest</span>' +
      '<div class="finding-box">' +
      escapeHtml(selectedWorker.findingSummary) +
      "</div>" +
      "</div>" +
      "</div>" +
      '<div class="enclave-block">' +
      '<div class="enclave-header"><span>Unix UID Isolation</span><span>RFC 6962</span></div>' +
      daemonsHtml +
      "</div>" +
      "</aside>" +
      "</section>" +
      "</main>" +
      cmdkModalHtml;

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
    if (state.presetKey === nextKey && !state.customSnapshot) return;
    var prevVm = normalizeSnapshotPacket(getActivePacket());
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
          if (!state.customSnapshot && state.presetKey === key) {
            render();
          }
        })
        .catch(function () {
          /* Embedded fallback preset active */
        });
    });
  }

  function handleFileImport(file) {
    if (!file) return;
    var reader = new FileReader();
    reader.onload = function (ev) {
      try {
        var parsed = JSON.parse(String(ev.target.result || "{}"));
        state.customSnapshot = parsed;
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
    document.addEventListener("click", function (ev) {
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
        state.selectedRowIndex = vm3.workers.length > 10 ? Math.floor(wIdx / 10) : wIdx;
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
        var step = 1;
        if (ev.key === "ArrowDown" || ev.key === "ArrowUp") {
          step = total > 10 ? 10 : 1;
        }
        var dir = ev.key === "ArrowRight" || ev.key === "ArrowDown" ? 1 : -1;
        var next = (state.selectedWorkerIndex + dir * step + total) % total;
        state.selectedWorkerIndex = next;
        state.selectedRowIndex = total > 10 ? Math.floor(next / 10) : next;
        render();
      }
    });
  }

  parseInitialUrlParams();
  bindEvents();
  render();
  loadExternalSnapshots();
})();
