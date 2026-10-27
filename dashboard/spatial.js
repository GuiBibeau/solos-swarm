(function () {
  "use strict";

  var stage, canvas, ctx, resizeObserver;
  var width = 0, height = 0, frame = 0, lastPaint = 0, lastTime = 0, elapsed = 0;
  var models = [], connections = [], hitTargets = [], layoutKey = "";
  var selected = 0, cohortIndices = new Set(), paused = false, formation = "orbit", onSelect;
  var introComplete = false, activityEnabled = true;
  var palette = {}, cameraPitch = 0.18, originalTouchAction;
  var pointer = { x: 0, y: 0, tx: 0, ty: 0 };
  var rotation = { yaw: 0, pitch: 0, velocity: 0 };
  var drag = null;
  var reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
  var stars = Array.from({ length: 960 }, function (_, index) {
    return { x: noise(index * 2 + 1), y: noise(index * 2 + 2), size: index % 41 === 0 ? 1.5 : 0.7, bucket: index % 4 };
  });

  function noise(seed) { var value = Math.sin(seed * 127.1) * 43758.5453; return value - Math.floor(value); }
  function clamp(value, min, max) { return Math.max(min, Math.min(max, value)); }
  function ease(value) { return 1 - Math.pow(1 - clamp(value, 0, 1), 4); }
  function rgba(alpha) { return "rgba(" + palette.ink + "," + alpha + ")"; }
  function metallic(value) { var v = Math.round(clamp(value, 0, 255)); return "rgb(" + v + "," + (v + 1) + "," + Math.min(255, v + 5) + ")"; }

  function readPalette() {
    var styles = getComputedStyle(document.documentElement);
    var light = document.documentElement.getAttribute("data-theme") === "light";
    palette = {
      ink: light ? "28,32,42" : "224,230,245",
      background: light ? "244,245,248" : "9,10,15",
      text: styles.getPropertyValue("--text-secondary").trim() || "#a1a1aa",
      light: light
    };
  }

  function line(points, color, lineWidth, close) {
    if (!points.length) return;
    ctx.beginPath();
    points.forEach(function (point, index) { ctx[index ? "lineTo" : "moveTo"](point.x, point.y); });
    if (close) ctx.closePath();
    ctx.strokeStyle = color;
    ctx.lineWidth = lineWidth || 0.65;
    ctx.stroke();
  }

  function polygon(points, fill, stroke) {
    ctx.beginPath();
    points.forEach(function (point, index) { ctx[index ? "lineTo" : "moveTo"](point.x, point.y); });
    ctx.closePath();
    ctx.fillStyle = fill;
    ctx.fill();
    if (stroke) { ctx.strokeStyle = stroke; ctx.lineWidth = 0.65; ctx.stroke(); }
  }

  function circle(x, y, radius, fill, stroke, lineWidth) {
    ctx.beginPath();
    ctx.arc(x, y, Math.max(0, radius), 0, Math.PI * 2);
    if (fill) { ctx.fillStyle = fill; ctx.fill(); }
    if (stroke) { ctx.strokeStyle = stroke; ctx.lineWidth = lineWidth || 0.7; ctx.stroke(); }
  }

  function paint() {
    if (!ctx || !width || !height) return;
    ctx.globalAlpha = 1;
    ctx.clearRect(0, 0, width, height);
    var centerX = width * (width > 760 && !(stage.classList && stage.classList.contains("is-expanded")) ? 0.585 : 0.5);
    var centerY = height * 0.51;
    var scale = Math.min(height / 14, width / (width > 760 ? 23 : 18));
    var yaw = -0.54 + elapsed * 0.065 + pointer.x * 0.10 + rotation.yaw;
    var pitch = cameraPitch + pointer.y * 0.07 + rotation.pitch;
    var cy = Math.cos(yaw), sy = Math.sin(yaw), cp = Math.cos(pitch), sp = Math.sin(pitch);
    var intro = introComplete ? 1 : ease(elapsed / 1.45);

    function project(x, y, z) {
      var rx = x * cy + z * sy;
      var rz = -x * sy + z * cy;
      var ry = y * cp - rz * sp;
      var depth = y * sp + rz * cp;
      var perspective = 19 / Math.max(5, 19 - depth);
      return { x: centerX + rx * scale * perspective, y: centerY - ry * scale * perspective, depth: depth, factor: perspective };
    }

    var aura = ctx.createRadialGradient(centerX, centerY, scale * 1.6, centerX, centerY, scale * 8);
    aura.addColorStop(0, rgba(palette.light ? 0.045 : 0.065));
    aura.addColorStop(0.5, rgba(palette.light ? 0.012 : 0.018));
    aura.addColorStop(1, rgba(0));
    ctx.fillStyle = aura;
    ctx.fillRect(0, 0, width, height);

    for (var bucket = 0; bucket < 4; bucket++) {
      ctx.fillStyle = rgba((palette.light ? 0.045 : 0.07) + bucket * 0.035);
      stars.forEach(function (star) {
        if (star.bucket !== bucket) return;
        var x = star.x * width + pointer.x * (bucket + 1) * 1.3;
        var y = star.y * height + pointer.y * (bucket + 1) * 1.3;
        ctx.fillRect(x, y, star.size, star.size);
      });
    }
    line([{ x: centerX - scale * 9.1, y: centerY }, { x: centerX + scale * 9.1, y: centerY }], rgba(0.055));
    line([{ x: centerX, y: centerY - scale * 6.9 }, { x: centerX, y: centerY + scale * 6.9 }], rgba(0.04));

    var nodes = models.map(function (model) {
      var progress = ease((elapsed - model.start - model.delay) / model.duration);
      var selectionProgress = ease((elapsed - model.selectionStart) / 0.55);
      model.x = model.from.x + (model.to.x - model.from.x) * progress;
      model.y = model.from.y + (model.to.y - model.from.y) * progress;
      model.z = model.from.z + (model.to.z - model.from.z) * progress;
      model.selection = model.fromSelection + (model.toSelection - model.fromSelection) * selectionProgress;
      var lift = 1 + model.selection * 0.055;
      var node = {
        x: model.x * lift, y: model.y * lift, z: model.z * lift,
        index: model.index, worker: model.worker,
        size: model.size * (1 + model.selection * 0.45),
        alpha: model.entering ? progress : 1, selection: model.selection
      };
      node.screen = project(node.x, node.y, node.z);
      return node;
    });
    var byDepth = nodes.slice().sort(function (a, b) { return a.screen.depth - b.screen.depth; });
    var coreRadius = scale * (formation === "lattice" ? 0.65 : 2.12);
    hitTargets = [];

    function ringPoint(angle, kind, radius) {
      if (kind === 1) {
        return { x: Math.cos(angle) * radius * 0.32, y: Math.sin(angle) * radius, z: Math.cos(angle) * radius * 0.96 };
      }
      var x = Math.cos(angle) * radius;
      var z = Math.sin(angle) * radius;
      return { x: x * 0.95, y: x * 0.35 + z * 0.5, z: z };
    }

    function drawRings(front) {
      [0, 1].forEach(function (kind) {
        var radius = kind === 0 ? 7.2 : 6.35;
        var alpha = kind === 0 ? 0.28 : 0.10;
        var points = [];
        for (var sample = 0; sample <= 200; sample++) {
          var angle = sample / 200 * Math.PI * 2;
          var p = ringPoint(angle, kind, radius);
          var screen = project(p.x, p.y, p.z);
          if ((screen.depth >= 0) === front) points.push(screen);
          else if (points.length) { line(points, rgba(alpha * intro), kind === 0 ? 0.8 : 0.6); points = []; }
        }
        line(points, rgba(alpha * intro), kind === 0 ? 0.8 : 0.6);
        if (kind !== 0) return;
        for (var tick = 0; tick < 120; tick++) {
          var a = tick / 120 * Math.PI * 2;
          var p1 = ringPoint(a, 0, radius + 0.08);
          var p2 = ringPoint(a, 0, radius + (tick % 10 === 0 ? 0.24 : 0.13));
          var screen1 = project(p1.x, p1.y, p1.z);
          if ((screen1.depth >= 0) === front) line([screen1, project(p2.x, p2.y, p2.z)], rgba((tick % 10 === 0 ? 0.45 : 0.20) * intro), 0.65);
        }
        var sweep = [];
        for (var s = 0; s <= 25; s++) {
          var p3 = ringPoint(elapsed * 0.26 + s / 25 * 0.65, 0, radius);
          var point = project(p3.x, p3.y, p3.z);
          if ((point.depth >= 0) === front) sweep.push(point);
        }
        line(sweep, rgba(0.84 * intro), 1.15);
        if (sweep.length) circle(sweep[sweep.length - 1].x, sweep[sweep.length - 1].y, 1.7, rgba(0.9 * intro));
      });
    }

    function drawConnections(front) {
      connections.forEach(function (edge) {
        var a = nodes[edge[0]], b = nodes[edge[1]];
        if (!a || !b) return;
        var depth = (a.screen.depth + b.screen.depth) / 2;
        if ((depth >= 0) !== front) return;
        var important = cohortIndices.has(a.index) && cohortIndices.has(b.index);
        var alpha = (important ? 0.42 : 0.075) * Math.min(a.alpha, b.alpha) * (front ? 1 : 0.5);
        line([a.screen, b.screen], rgba(alpha), important ? 0.8 : 0.5);
      });
    }

    function drawPaths(front) {
      var origin = nodes[selected];
      if (!origin) return;
      var cohort = nodes.filter(function (node) { return node.index !== selected && cohortIndices.has(node.index); }).slice(0, 10);
      cohort.forEach(function (node, index) {
        var points = [], packet;
        var phase = (elapsed * 0.19 + index * 0.173) % 1;
        function sample(t) {
          var bend = Math.sin(t * Math.PI) * 0.9;
          var x = origin.x * (1 - t) + node.x * t;
          var y = origin.y * (1 - t) + node.y * t + bend;
          var z = origin.z * (1 - t) + node.z * t;
          return project(x, y, z);
        }
        for (var i = 0; i <= 24; i++) {
          var p = sample(i / 24);
          if ((p.depth >= 0) === front) points.push(p);
          else if (points.length) { line(points, rgba(0.29 * intro), 0.65); points = []; }
        }
        line(points, rgba(0.29 * intro), 0.65);
        if (!activityEnabled) return;
        packet = sample(phase);
        if ((packet.depth >= 0) !== front) return;
        var trail = [];
        for (var t = 0; t < 6; t++) trail.push(sample(Math.max(0, phase - t * 0.012)));
        line(trail, rgba(0.65 * intro), 1.1);
        circle(packet.x, packet.y, 1.45, rgba(0.95 * intro));
      });
    }

    function drawCube(node) {
      var active = node.index === selected;
      var inCohort = cohortIndices.has(node.index);
      var depthLight = clamp((node.screen.depth + 6) / 12, 0, 1);
      var alpha = node.alpha * (0.25 + depthLight * 0.75);
      var half = node.size;
      var points = [
        [-1, -1, -1], [1, -1, -1], [1, 1, -1], [-1, 1, -1],
        [-1, -1, 1], [1, -1, 1], [1, 1, 1], [-1, 1, 1]
      ].map(function (v) { return project(node.x + v[0] * half, node.y + v[1] * half, node.z + v[2] * half); });
      var faces = [[4, 5, 6, 7], [1, 0, 3, 2], [5, 1, 2, 6], [0, 4, 7, 3], [3, 7, 6, 2], [0, 1, 5, 4]];
      var faceLight = [0.78, 0.50, 0.43, 0.76, 1.18, 0.32];
      ctx.globalAlpha = alpha;
      if (active || (inCohort && node.screen.depth > 0)) {
        var halo = ctx.createRadialGradient(node.screen.x, node.screen.y, 0, node.screen.x, node.screen.y, scale * half * node.screen.factor * 4.5);
        halo.addColorStop(0, rgba(active ? 0.25 : 0.09)); halo.addColorStop(1, rgba(0));
        circle(node.screen.x, node.screen.y, scale * half * node.screen.factor * 4.5, halo);
      }
      faces.forEach(function (face, index) {
        var p = face.map(function (vertex) { return points[vertex]; });
        var area = (p[1].x - p[0].x) * (p[2].y - p[0].y) - (p[1].y - p[0].y) * (p[2].x - p[0].x);
        if (area >= 0) return;
        var shine = Math.max(0, Math.cos(node.x * 0.2 + node.z * 0.23 - elapsed * 0.16));
        var base = active ? (palette.light ? 65 : 225) : (palette.light ? 170 : 108 + depthLight * 65 + (inCohort ? 20 : 0));
        var value = base * faceLight[index] + shine * 14;
        var chrome = ctx.createLinearGradient(p[0].x, p[0].y, p[2].x, p[2].y);
        chrome.addColorStop(0, metallic(value * 0.58));
        chrome.addColorStop(0.30, metallic(value * 0.92));
        chrome.addColorStop(0.48, metallic(value * 1.34));
        chrome.addColorStop(0.55, metallic(value * 1.05));
        chrome.addColorStop(1, metallic(value * 0.48));
        polygon(p, chrome, rgba(active ? 0.95 : 0.32 + depthLight * 0.38));
        if (index === 4) line([p[0], p[1]], rgba(active ? 1 : 0.52 + shine * 0.38), 0.85);
      });
      ctx.globalAlpha = 1;
      var hiddenByCore = node.screen.depth < 0 && Math.hypot(node.screen.x - centerX, node.screen.y - centerY) < coreRadius;
      if (!hiddenByCore && node.alpha > 0.45) hitTargets.push({ index: node.index, x: node.screen.x, y: node.screen.y, radius: Math.max(9, half * scale * node.screen.factor * 1.9), depth: node.screen.depth });
    }

    drawRings(false);
    drawConnections(false);
    drawPaths(false);
    byDepth.forEach(function (node) { if (node.screen.depth < 0) drawCube(node); });

    if (models.length) {
      var halo = ctx.createRadialGradient(centerX, centerY, coreRadius * 0.8, centerX, centerY, coreRadius * 2.2);
      halo.addColorStop(0, rgba(0.07 * intro)); halo.addColorStop(0.45, rgba(0.025 * intro)); halo.addColorStop(1, rgba(0));
      circle(centerX, centerY, coreRadius * 2.2, halo);
      var nucleus = ctx.createRadialGradient(centerX - coreRadius * 0.42, centerY - coreRadius * 0.45, coreRadius * 0.03, centerX, centerY, coreRadius);
      nucleus.addColorStop(0, palette.light ? "#e2e5ed" : "#252b37");
      nucleus.addColorStop(0.4, palette.light ? "#d6dbe5" : "#121722");
      nucleus.addColorStop(0.83, palette.light ? "#b5bdcc" : "#080b11");
      nucleus.addColorStop(0.97, palette.light ? "#919bad" : "#1d2431");
      nucleus.addColorStop(1, palette.light ? "#cbd0d9" : "#5b6576");
      ctx.globalAlpha = intro;
      circle(centerX, centerY, coreRadius, nucleus, rgba(0.35), 0.7);
      ctx.save();
      ctx.beginPath(); ctx.arc(centerX, centerY, coreRadius - 0.7, 0, Math.PI * 2); ctx.clip();
      for (var latitude = -2; latitude <= 2; latitude++) {
        var lat = latitude * 0.43;
        var radius = 2.12 * Math.cos(lat);
        var y = 2.12 * Math.sin(lat);
        var wire = [];
        for (var seg = 0; seg <= 100; seg++) {
          var angle = seg / 100 * Math.PI * 2;
          var spherePoint = project(Math.cos(angle) * radius, y, Math.sin(angle) * radius);
          if (spherePoint.depth > 0) wire.push(spherePoint);
          else if (wire.length) { line(wire, rgba(0.08), 0.5); wire = []; }
        }
        line(wire, rgba(0.08), 0.5);
      }
      var gloss = ctx.createLinearGradient(centerX - coreRadius, centerY - coreRadius, centerX + coreRadius, centerY + coreRadius);
      gloss.addColorStop(0, rgba(0.6)); gloss.addColorStop(0.4, rgba(0.08)); gloss.addColorStop(0.8, rgba(0.015)); gloss.addColorStop(1, rgba(0.2));
      circle(centerX, centerY, coreRadius - 0.7, null, gloss, 1.1);
      ctx.restore();
      ctx.globalAlpha = 1;
    }

    drawConnections(true);
    drawPaths(true);
    byDepth.forEach(function (node) { if (node.screen.depth >= 0) drawCube(node); });
    drawRings(true);

    var active = nodes[selected];
    if (active && active.alpha > 0.45) {
      var p = active.screen;
      var radius = active.size * scale * p.factor * 2.1 + 5;
      var span = 5;
      [[-1, -1], [1, -1], [1, 1], [-1, 1]].forEach(function (corner) {
        var x = p.x + corner[0] * radius, y = p.y + corner[1] * radius;
        line([{ x: x - corner[0] * span, y: y }, { x: x, y: y }, { x: x, y: y - corner[1] * span }], rgba(0.9 * active.alpha), 0.8);
      });
      var labelX = clamp(p.x + radius + 15, 20, width - 90);
      var labelY = clamp(p.y - radius - 14, 80, height - 40);
      line([{ x: p.x + radius, y: p.y - radius }, { x: labelX - 5, y: labelY }, { x: labelX + 58, y: labelY }], rgba(0.55 * active.alpha), 0.65);
      ctx.fillStyle = palette.text;
      ctx.font = "9px ui-monospace, SFMono-Regular, Menlo, monospace";
      ctx.textAlign = "left";
      ctx.fillText(active.worker.workerId, labelX, labelY - 5);
    }
  }

  function destination(index, count, worker, maxDuration) {
    if (formation === "lattice") {
      var columns = Math.ceil(Math.sqrt(count));
      var rows = Math.ceil(count / columns);
      var spacing = count > 10 ? 1.04 : 2.1;
      return { x: (index % columns - (columns - 1) / 2) * spacing, y: -1.1 + Math.sqrt(worker.durationMs / maxDuration) * 1.2, z: (Math.floor(index / columns) - (rows - 1) / 2) * spacing };
    }
    if (formation === "helix") {
      var step = Math.floor(index / 2) / Math.max(1, Math.ceil(count / 2) - 1);
      var angle = step * Math.PI * 3.6 + index % 2 * Math.PI;
      return { x: Math.cos(angle) * 3.3, y: (step - 0.5) * 9.8, z: Math.sin(angle) * 3.3 };
    }
    var y = 1 - 2 * (index + 0.5) / Math.max(1, count);
    var radial = Math.sqrt(Math.max(0, 1 - y * y));
    var phi = index * Math.PI * (3 - Math.sqrt(5));
    var radius = 5.12 + Math.sqrt(worker.durationMs / maxDuration) * 0.3;
    return { x: Math.cos(phi) * radial * radius, y: y * radius, z: Math.sin(phi) * radial * radius };
  }

  function rebuildConnections() {
    connections = [];
    models.forEach(function (model, index) {
      var neighbors = models.map(function (other, otherIndex) {
        return { index: otherIndex, distance: Math.hypot(model.to.x - other.to.x, model.to.y - other.to.y, model.to.z - other.to.z) };
      }).filter(function (other) { return other.index !== index; }).sort(function (a, b) { return a.distance - b.distance; }).slice(0, formation === "helix" ? 2 : 3);
      neighbors.forEach(function (other) { if (other.index > index) connections.push([index, other.index]); });
    });
  }

  function shouldAnimate() { return stage && !paused && !reducedMotion.matches && !document.hidden; }

  function animate(now) {
    frame = 0;
    if (!shouldAnimate()) return;
    if (now - lastPaint >= 25) {
      var dt = lastTime ? Math.min((now - lastTime) / 1000, 0.1) : 0;
      elapsed += dt; lastTime = now; lastPaint = now;
      pointer.x += (pointer.tx - pointer.x) * 0.12;
      pointer.y += (pointer.ty - pointer.y) * 0.12;
      cameraPitch += ((formation === "lattice" ? 0.65 : 0.18) - cameraPitch) * 0.065;
      if (!drag) { rotation.yaw += rotation.velocity; rotation.velocity *= 0.92; }
      paint();
    }
    frame = requestAnimationFrame(animate);
  }

  function syncAnimation() {
    if (frame) cancelAnimationFrame(frame);
    frame = 0; lastTime = 0;
    if (paused || reducedMotion.matches) {
      introComplete = true;
      models.forEach(function (model) { model.start = elapsed - 3; model.selectionStart = elapsed - 3; model.entering = false; });
      cameraPitch = formation === "lattice" ? 0.65 : 0.18;
      rotation.velocity = 0;
    }
    paint();
    if (shouldAnimate()) frame = requestAnimationFrame(animate);
  }

  function resize() {
    if (!stage) return;
    var rect = stage.getBoundingClientRect(); width = rect.width; height = rect.height;
    var dpr = Math.min(window.devicePixelRatio || 1, 2);
    canvas.width = Math.round(width * dpr); canvas.height = Math.round(height * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    paint();
  }

  function isControl(target) { return target && target.closest && target.closest("button, a, input, select, textarea"); }

  function movePointer(event) {
    if (!stage || isControl(event.target)) return;
    var rect = stage.getBoundingClientRect();
    if (!paused && !reducedMotion.matches && event.pointerType !== "touch") {
      pointer.tx = clamp((event.clientX - rect.left) / rect.width * 2 - 1, -1, 1);
      pointer.ty = clamp((event.clientY - rect.top) / rect.height * 2 - 1, -1, 1);
    }
    if (!drag || event.pointerId !== drag.id) return;
    if (Math.hypot(event.clientX - drag.x, event.clientY - drag.y) > 5) drag.moved = true;
    if (reducedMotion.matches) return;
    var dx = event.clientX - drag.lastX, dy = event.clientY - drag.lastY;
    if (event.pointerType === "touch" && Math.abs(event.clientY - drag.y) > Math.abs(event.clientX - drag.x) * 1.3) return;
    rotation.yaw += dx * 0.007;
    rotation.pitch = clamp(rotation.pitch + dy * 0.004, -0.8, 0.8);
    rotation.velocity = clamp(dx * 0.004, -0.09, 0.09);
    drag.lastX = event.clientX; drag.lastY = event.clientY;
    if (paused) paint();
  }

  function pointerDown(event) {
    if (isControl(event.target) || (event.button !== undefined && event.button !== 0)) return;
    drag = { id: event.pointerId, x: event.clientX, y: event.clientY, lastX: event.clientX, lastY: event.clientY, moved: false };
    rotation.velocity = 0;
    if (stage.setPointerCapture) stage.setPointerCapture(event.pointerId);
  }

  function pointerUp(event) {
    if (!drag || event.pointerId !== drag.id) return;
    var clicked = !drag.moved && !isControl(event.target);
    drag = null;
    if (paused || reducedMotion.matches) rotation.velocity = 0;
    if (stage.hasPointerCapture && stage.hasPointerCapture(event.pointerId)) stage.releasePointerCapture(event.pointerId);
    if (!clicked || !onSelect) return;
    var rect = stage.getBoundingClientRect();
    var x = event.clientX - rect.left, y = event.clientY - rect.top;
    var hit = hitTargets.filter(function (target) { return Math.hypot(target.x - x, target.y - y) <= target.radius; }).sort(function (a, b) { return b.depth - a.depth; })[0];
    if (hit) onSelect(hit.index);
  }

  function resetPointer() { pointer.tx = pointer.ty = 0; }
  function cancelDrag() { drag = null; rotation.velocity = 0; }

  function dispose() {
    if (frame) cancelAnimationFrame(frame);
    frame = 0;
    if (resizeObserver) resizeObserver.disconnect();
    if (stage) {
      stage.removeEventListener("pointermove", movePointer); stage.removeEventListener("pointerleave", resetPointer);
      stage.removeEventListener("pointerdown", pointerDown); stage.removeEventListener("pointerup", pointerUp); stage.removeEventListener("pointercancel", cancelDrag); stage.removeEventListener("lostpointercapture", cancelDrag);
      stage.style.touchAction = originalTouchAction;
    }
    document.removeEventListener("visibilitychange", syncAnimation); reducedMotion.removeEventListener("change", syncAnimation);
    if (canvas) canvas.remove();
    stage = canvas = ctx = resizeObserver = null;
    models = []; connections = []; hitTargets = []; layoutKey = ""; onSelect = null;
    lastTime = lastPaint = elapsed = 0;
    cameraPitch = 0.18; introComplete = false; rotation.yaw = rotation.pitch = rotation.velocity = 0;
    pointer.x = pointer.y = pointer.tx = pointer.ty = 0; drag = null;
  }

  function mount(container) {
    if (stage === container) return;
    dispose(); stage = container;
    canvas = document.createElement("canvas"); canvas.className = "spatial-canvas"; canvas.setAttribute("aria-hidden", "true");
    canvas.style.cssText = "position:absolute;inset:0;width:100%;height:100%;pointer-events:none";
    stage.appendChild(canvas); ctx = canvas.getContext("2d");
    if (!ctx) return;
    originalTouchAction = stage.style.touchAction; stage.style.touchAction = "pan-y";
    readPalette();
    stage.addEventListener("pointermove", movePointer, { passive: true }); stage.addEventListener("pointerleave", resetPointer, { passive: true });
    stage.addEventListener("pointerdown", pointerDown); stage.addEventListener("pointerup", pointerUp); stage.addEventListener("pointercancel", cancelDrag); stage.addEventListener("lostpointercapture", cancelDrag);
    document.addEventListener("visibilitychange", syncAnimation); reducedMotion.addEventListener("change", syncAnimation);
    resizeObserver = new ResizeObserver(resize); resizeObserver.observe(stage); resize(); syncAnimation();
  }

  function update(next) {
    var nextWorkers = (next.workers || []).map(function (worker) {
      var duration = Number(worker.durationMs);
      return { workerId: String(worker.workerId || ""), durationMs: Number.isFinite(duration) ? Math.max(0, duration) : 0 };
    });
    var nextSelected = next.selectedWorkerIndex || 0;
    formation = ["orbit", "lattice", "helix"].indexOf(next.formation) !== -1 ? next.formation : "orbit";
    var nextKey = formation + ":" + nextWorkers.map(function (worker) { return worker.workerId + ":" + worker.durationMs; }).join("|");
    if (nextKey !== layoutKey) {
      var previous = new Map(models.map(function (model) { return [model.worker.workerId, model]; }));
      var maxDuration = nextWorkers.reduce(function (max, worker) { return Math.max(max, worker.durationMs); }, 1);
      models = nextWorkers.map(function (worker, index) {
        var prior = previous.get(worker.workerId);
        var target = destination(index, nextWorkers.length, worker, maxDuration);
        var source = prior ? { x: prior.x, y: prior.y, z: prior.z } : { x: target.x * 0.2, y: target.y * 0.2, z: target.z * 0.2 };
        return {
          worker: worker, index: index, from: source, to: target, x: source.x, y: source.y, z: source.z,
          size: (nextWorkers.length > 10 ? 0.13 : 0.31) + Math.sqrt(worker.durationMs / maxDuration) * (nextWorkers.length > 10 ? 0.055 : 0.10),
          start: elapsed, delay: prior ? 0 : index / Math.max(1, nextWorkers.length) * 0.4,
          duration: prior ? 1.2 : 1.35, entering: !prior,
          selection: prior ? prior.selection : 0, fromSelection: prior ? prior.selection : 0,
          toSelection: index === nextSelected ? 1 : 0, selectionStart: elapsed
        };
      });
      layoutKey = nextKey; rebuildConnections();
    } else if (nextSelected !== selected) {
      models.forEach(function (model) { model.fromSelection = model.selection; model.toSelection = model.index === nextSelected ? 1 : 0; model.selectionStart = elapsed; });
    }
    selected = nextSelected;
    var perRow = nextWorkers.length > 10 ? Math.ceil(nextWorkers.length / 10) : 1;
    var activeRow = next.selectedRowIndex || 0;
    cohortIndices = new Set(Array.isArray(next.cohortIndices) ? next.cohortIndices : nextWorkers.map(function (_, index) { return index; }).filter(function (index) { return Math.floor(index / perRow) === activeRow; }));
    if (typeof next.onSelect === "function") onSelect = next.onSelect;
    activityEnabled = next.activityEnabled !== false;
    paused = Boolean(next.paused); readPalette(); syncAnimation();
  }

  window.SwarmSpatial = { mount: mount, update: update, dispose: dispose };
})();
