import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const context = { window: {} };
vm.runInNewContext(readFileSync(new URL('../dashboard/telemetry.js', import.meta.url), 'utf8'), context);
const normalize = raw => JSON.parse(JSON.stringify(context.window.SwarmTelemetry.normalize(raw)));

function packet(count = 20) {
  return {
    schema: 'solos-swarm.telemetry/v1', revision: 0,
    source: { kind: 'runtime-file', label: 'local-run', updated_at: 5000, connected: true, service_active: false },
    config: { goal_id: 'real-goal', intent: 'Run configured work', max_agents: 100, budget_microusd: 100_000_000, model: 'configured-model', provider: 'local', capabilities: ['claim_task'] },
    state: {
      goal_status: 'completed', spent_microusd: 0, held_microusd: 0,
      agents: Object.fromEntries(Array.from({ length: count }, (_, index) => [`agent-${index}`, { started_ms: 1000 + index, stopped_ms: 2500 + index, model_calls: 0, status: 'stopped' }])),
      tasks: Array.from({ length: count }, (_, index) => ({ id: `task-${index}`, claimed_by: `agent-${index}`, status: index % 2 ? 'done' : { Completed: null }, fencing_token: index })),
      intents: [], findings: []
    }
  };
}

test('actual twenty and thirty agents stay distinct from configured capacity and preserve real budget', () => {
  for (const count of [20, 30]) {
    const result = normalize(packet(count));
    assert.equal(result.workerCount, count);
    assert.equal(result.workers.length, count);
    assert.equal(result.configuredWorkers, 100);
    assert.equal(result.ceilingMicro, 100_000_000);
    assert.equal(result.completedShards, count);
    assert.equal(result.timelineRows.length, 10);
    assert.deepEqual(result.timelineRows.flatMap(row => row.workerIndices), Array.from({ length: count }, (_, index) => index));
    assert.equal(result.totalMs, 1500 + count - 1);
    assert.equal(result.parallelMs, 1500);
    assert.equal(result.synthMs, null);
    assert.equal(result.workers[0].fence, 0);
    assert.equal(result.model, 'configured-model');
    assert.equal(result.provider, 'local');
    assert.deepEqual(result.daemons, []);
    assert.equal(result.keysInWorkers, null);
  }
});

test('zero workers and explicit zeros remain zero without manufacturing workers or timing', () => {
  const raw = packet(0);
  raw.config.budget_microusd = 0;
  raw.config.max_agents = 0;
  raw.config.keys_in_workers = 0;
  const result = normalize(raw);
  for (const field of ['workerCount', 'configuredWorkers', 'ceilingMicro', 'settledMicro', 'heldMicro', 'modelCalls', 'totalTokens', 'unknownHolds', 'completedShards', 'keysInWorkers', 'revision']) assert.equal(result[field], 0, field);
  assert.equal(result.totalMs, null);
  assert.equal(result.parallelMs, null);
  assert.deepEqual(result.workers, []);
  assert.deepEqual(result.timelineRows, []);
});

test('missing telemetry stays unknown and stopped agents do not imply completed tasks', () => {
  const result = normalize({ schema: 'solos-swarm.telemetry/v1', state: { agents: { actual: { status: 'stopped' } }, tasks: [{ id: 't', claimed_by: 'actual', status: 'failed' }] } });
  for (const field of ['configuredWorkers', 'ceilingMicro', 'settledMicro', 'heldMicro', 'modelCalls', 'totalTokens', 'totalMs', 'parallelMs', 'synthMs', 'keysInWorkers', 'unknownHolds', 'revision']) assert.equal(result[field], null, field);
  assert.equal(result.completedShards, 0);
  assert.equal(result.statusLabel, 'UNKNOWN');
  assert.equal(result.source.service_active, null);
  assert.equal(result.workers[0].calls, null);
  assert.equal(result.workers[0].tokens, null);
  assert.equal(result.workers[0].settledMicro, null);
  assert.equal(result.workers[0].durationMs, null);
  assert.equal(result.workers[0].findingSummary, 'No finding recorded');
});

test('service state prevents stale running flags from inventing elapsed time', () => {
  const raw = packet(1);
  raw.state.goal_status = 'running';
  raw.state.agents['agent-0'] = { started_ms: 1000, stopped_ms: null, status: 'running' };
  const stopped = normalize(raw);
  assert.equal(stopped.statusLabel, 'STOPPED');
  assert.equal(stopped.totalMs, null);
  assert.equal(stopped.workers[0].durationMs, null);
  raw.source.service_active = true;
  const running = normalize(raw);
  assert.equal(running.statusLabel, 'RUNNING');
  assert.equal(running.totalMs, 4000);
  assert.equal(running.workers[0].durationMs, 4000);
  raw.source.service_active = null;
  assert.equal(normalize(raw).totalMs, null);
});

test('actual intent receipts and findings are attributed without synthetic summaries or partial totals', () => {
  const raw = packet(2);
  delete raw.state.agents['agent-0'].model_calls;
  raw.state.intents = [
    { agent_id: 'agent-0', state: 'completed', cost_microusd: 0, usage: { input_tokens: 0, output_tokens: 0 } },
    { agent_id: 'agent-1', state: { Settled: null }, cost_microusd: 43, usage: { input_tokens: 10, output_tokens: 3 } },
    { agent_id: 'agent-1', state: { UnknownHold: null }, cost_microusd: null, usage: null }
  ];
  raw.state.findings = [{ agent_id: 'agent-0', summary: 'Actual finding' }, { task_id: 'task-1', has_result: true }];
  const result = normalize(raw);
  assert.equal(result.modelCalls, null);
  assert.equal(result.totalTokens, 13);
  assert.equal(result.unknownHolds, 1);
  assert.equal(result.workers[0].calls, null);
  assert.equal(result.workers[0].tokens, 0);
  assert.equal(result.workers[0].settledMicro, 0);
  assert.equal(result.workers[1].tokens, 13);
  assert.equal(result.workers[1].settledMicro, 43);
  assert.equal(result.workers[0].findingSummary, 'Actual finding');
  assert.equal(result.workers[1].findingSummary, 'Finding recorded');
  delete raw.state.intents[1].usage.output_tokens;
  assert.equal(normalize(raw).totalTokens, null);
  assert.equal(normalize(raw).workers[1].tokens, null);
});

test('legacy runtime JSON is recognized while bundled snapshots keep their original normalizer', () => {
  const raw = packet(1).state;
  raw.schema_version = 1;
  raw.configuration = { max_agents: 20, budget_microusd: 0 };
  raw.findings = [{ agent_id: 'agent-0', result: { recorded: true } }];
  const result = normalize(raw);
  assert.equal(result.isRealData, true);
  assert.equal(result.configuredWorkers, 20);
  assert.equal(result.ceilingMicro, 0);
  assert.equal(result.source.label, 'Snapshot');
  assert.equal(result.source.connected, false);
  assert.equal(result.workers[0].findingSummary, 'Finding recorded');
  assert.equal(normalize({ schema: 'solos-swarm.snapshot/v1', snapshot: {} }), null);
  assert.equal(normalize({ observation: { workers: [] } }), null);
  assert.equal(normalize(null), null);
});


test('denied and reserved intents never invent requests, tokens, or unknown receipt costs', () => {
  const raw = packet(1);
  raw.state.intents = [
    { agent_id: 'agent-0', state: 'denied', cost_microusd: null, usage: null },
    { agent_id: 'agent-0', state: 'reserved', hold_microusd: 200, cost_microusd: null, usage: null },
    { agent_id: 'agent-0', state: 'completed', cost_microusd: 75, usage: { input_tokens: 50, output_tokens: 7 } }
  ];
  let result = normalize(raw);
  assert.equal(result.workers[0].settledMicro, 75);
  assert.equal(result.workers[0].tokens, 57);
  assert.equal(result.totalTokens, 57);
  assert.equal(result.workers[0].calls, 0, 'explicit request counters remain authoritative');
  assert.equal(result.modelCalls, 0);
  delete raw.state.agents['agent-0'].model_calls;
  result = normalize(raw);
  assert.equal(result.modelCalls, null, 'intent admission count cannot stand in for model requests');
  assert.equal(result.workers[0].calls, null);
  delete raw.state.intents[2].cost_microusd;
  assert.equal(normalize(raw).workers[0].settledMicro, null, 'a missing completed receipt cost stays unknown');
  delete raw.state.intents[2].usage;
  assert.equal(normalize(raw).totalTokens, null, 'a missing completed usage receipt stays unknown');
});
