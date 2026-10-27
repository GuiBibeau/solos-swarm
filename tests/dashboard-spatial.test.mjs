import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

function harness() {
  const listeners = new Map();
  const frames = new Map();
  let nextFrame = 0;
  let removed = false;
  let disconnected = false;
  let fills = [];
  let vertices = 0, closed = false, radius = null, smallCircles = 0;
  const reducedMotion = { matches: false, addEventListener: (_, fn) => listeners.set('motion', fn), removeEventListener: () => listeners.delete('motion') };
  const ctx = new Proxy({
    globalAlpha: 1,
    createRadialGradient: () => ({ addColorStop() {} }),
    createLinearGradient: () => ({ addColorStop() {} }),
    clearRect() { fills = []; smallCircles = 0; },
    beginPath() { vertices = 0; closed = false; radius = null; },
    arc(_x, _y, value) { radius = value; },
    closePath() { closed = true; },
    fill() { if (closed && vertices === 4) fills.push(this.globalAlpha); if (radius !== null && radius < 2) smallCircles++; },
    moveTo(x, y) { vertices++; assert(Number.isFinite(x) && Number.isFinite(y), 'finite projected geometry'); },
    lineTo(x, y) { vertices++; assert(Number.isFinite(x) && Number.isFinite(y), 'finite projected geometry'); }
  }, { get: (target, key) => key in target ? target[key] : () => {}, set: (target, key, value) => (target[key] = value, true) });
  const canvas = { style: {}, setAttribute() {}, getContext: () => ctx, remove() { removed = true; } };
  const stage = { style: {}, getBoundingClientRect: () => ({ width: 1342, height: 300, left: 0, top: 0 }), appendChild() {}, addEventListener: (name, fn) => listeners.set(name, fn), removeEventListener: name => listeners.delete(name) };
  const document = { hidden: false, documentElement: { getAttribute: () => null }, createElement: () => canvas, addEventListener: (name, fn) => listeners.set(name, fn), removeEventListener: name => listeners.delete(name) };
  const window = { matchMedia: () => reducedMotion, devicePixelRatio: 3 };
  vm.runInNewContext(readFileSync(new URL('../dashboard/spatial.js', import.meta.url), 'utf8'), {
    window, document, getComputedStyle: () => ({ getPropertyValue: () => '#a1a1aa' }),
    requestAnimationFrame: fn => { frames.set(++nextFrame, fn); return nextFrame; },
    cancelAnimationFrame: id => frames.delete(id),
    ResizeObserver: class { observe() {} disconnect() { disconnected = true; } }
  });
  const scene = window.SwarmSpatial;
  scene.mount(stage);
  return {
    scene, frames, canvas, document, reducedMotion, listeners,
    get fills() { return fills; },
    get smallCircles() { return smallCircles; },
    get cleaned() { return removed && disconnected && listeners.size === 0; },
    tick(now) { const pending = [...frames.values()]; frames.clear(); pending.forEach(fn => fn(now)); }
  };
}

const workers = Array.from({ length: 100 }, (_, index) => ({ workerId: `agent-${index}`, durationMs: 1000 + index * 31 }));

test('spatial renderer caps resolution and owns one stoppable, disposable animation loop', () => {
  const h = harness();
  h.scene.update({ workers });
  assert.equal(h.canvas.width, 2684, 'DPR is capped at 2');
  assert.equal(h.frames.size, 1);
  h.scene.update({ workers, paused: true });
  assert.equal(h.frames.size, 0);
  h.scene.update({ workers, paused: false });
  h.reducedMotion.matches = true;
  h.listeners.get('motion')();
  assert.equal(h.frames.size, 0, 'live reduced-motion changes stop the loop');
  h.reducedMotion.matches = false;
  h.listeners.get('motion')();
  assert.equal(h.frames.size, 1);
  h.document.hidden = true;
  h.listeners.get('visibilitychange')();
  assert.equal(h.frames.size, 0, 'hidden documents stop the loop');
  h.scene.dispose();
  assert(h.cleaned, 'dispose removes canvas, observer, and listeners');
});

test('assembly settles once, survives selection and malformed or empty snapshot geometry', () => {
  const h = harness();
  h.scene.update({ workers });
  assert.equal(h.fills.length, 300, 'each worker has three extruded faces');
  assert(h.fills.some(alpha => alpha < 1), 'workers enter from the plane');
  for (let time = 33; time <= 2200; time += 33) h.tick(time);
  assert(h.fills.every(alpha => alpha > 0.1), 'assembly settles completely');
  h.scene.update({ workers, selectedWorkerIndex: 44, cohortIndices: [43, 44, 45] });
  assert(h.fills.every(alpha => alpha > 0.1), 'selection does not replay assembly');
  h.scene.update({ workers: [], paused: true });
  assert.equal(h.fills.length, 0);
  h.scene.update({ workers: [{ workerId: 'imported', durationMs: Infinity }], paused: true });
  assert.equal(h.fills.length, 3);
  h.scene.dispose();
});


test('formation changes retain workers and direct manipulation selects a visible node', () => {
  const h = harness();
  const inspected = [];
  const state = { workers, paused: true, onSelect: index => inspected.push(index) };
  ['orbit', 'lattice', 'helix'].forEach(formation => {
    h.scene.update({ ...state, formation });
    assert(h.fills.length >= 200 && h.fills.length <= 300, 'workers retain their visible solid faces');
    assert.equal(h.frames.size, 0, 'paused formations render a complete static scene');
  });
  const target = { closest: () => false };
  let hitEvent;
  for (let y = 15; y < 300 && !inspected.length; y += 10) {
    for (let x = 350; x < 1250 && !inspected.length; x += 10) {
      const event = { target, pointerId: 1, clientX: x, clientY: y, button: 0 };
      h.listeners.get('pointerdown')(event);
      h.listeners.get('pointerup')(event);
      if (inspected.length) hitEvent = event;
    }
  }
  assert(inspected.length > 0, 'clicking visible geometry calls onSelect');
  assert(inspected[0] >= 0 && inspected[0] < workers.length);
  const control = { closest: () => true };
  const count = inspected.length;
  h.listeners.get('pointerdown')({ target: control, pointerId: 1, clientX: 700, clientY: 150 });
  h.listeners.get('pointerup')({ target: control, pointerId: 1, clientX: 700, clientY: 150 });
  assert.equal(inspected.length, count, 'scene controls do not select geometry');
  h.reducedMotion.matches = true;
  h.listeners.get('motion')();
  h.listeners.get('pointerdown')({ ...hitEvent, clientX: hitEvent.clientX - 12 });
  h.listeners.get('pointermove')(hitEvent);
  h.listeners.get('pointerup')(hitEvent);
  assert.equal(inspected.length, count, 'reduced-motion swipes do not become accidental clicks');
  assert.equal(h.frames.size, 0);
  h.scene.dispose();
});


test('inactive telemetry retains cohort geometry while suppressing moving packet markers', () => {
  const h = harness();
  const state = { workers, paused: true, cohortIndices: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9] };
  h.scene.update({ ...state, activityEnabled: true });
  const activeMarkers = h.smallCircles;
  const faceCount = h.fills.length;
  h.scene.update({ ...state, activityEnabled: false });
  assert(h.smallCircles < activeMarkers, 'processing markers are suppressed');
  assert.equal(h.fills.length, faceCount, 'the retained snapshot still renders all worker geometry');
  h.scene.dispose();
});
