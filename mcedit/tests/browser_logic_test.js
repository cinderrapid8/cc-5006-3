// Headless logic tests for the browser-side timeline code (app.js).
// Runs under Node without a DOM: the pure functions are extracted by loading
// app.js inside a minimal stub that swallows the DOM-dependent tail.
"use strict";

const fs = require("fs");
const path = require("path");
const vm = require("vm");
const assert = require("assert");

const source = fs.readFileSync(path.join(__dirname, "..", "web", "app.js"), "utf8");

function makeEl() {
  return {
    addEventListener: () => {},
    querySelector: () => makeEl(),
    querySelectorAll: () => [],
    appendChild: () => {},
    classList: { toggle: () => {}, add: () => {}, remove: () => {} },
    style: {},
    dataset: {},
    innerHTML: "",
    textContent: "",
    value: "",
    showModal: () => {},
    getContext: () => ({ fillRect: () => {}, drawImage: () => {}, fillText: () => {} }),
    width: 640,
    height: 360,
  };
}

// Stub just enough browser surface for app.js to evaluate.
const sandbox = {
  console,
  localStorage: {
    getItem: () => null,
    setItem: () => {},
  },
  document: {
    getElementById: () => makeEl(),
    createElement: () => makeEl(),
    querySelectorAll: () => [],
    querySelector: () => makeEl(),
  },
  fetch: async () => ({ ok: false, json: async () => ({}) }),
  requestAnimationFrame: () => 0,
  cancelAnimationFrame: () => {},
  performance: { now: () => 0 },
  alert: (m) => {
    throw new Error("unexpected alert: " + m);
  },
  confirm: () => true,
};
sandbox.window = sandbox;
vm.createContext(sandbox);
vm.runInContext(source, sandbox);
const api = sandbox.window.mcedit;
const { fitMapping, mappingIsUncertain, rangeHalfWidth } = api;

const manifest = JSON.parse(
  fs.readFileSync(path.join(__dirname, "..", "data", "media-manifest.json"), "utf8")
);
const project = JSON.parse(
  fs.readFileSync(path.join(__dirname, "..", "data", "projects", "news-main.json"), "utf8")
);

let passed = 0;
function check(name, fn) {
  fn();
  passed += 1;
  console.log("ok -", name);
}

check("mapping pure offset mirrors Rust convention", () => {
  const host = project.sources.find((s) => s.id === "host");
  const m = api.fitMapping(host.anchors);
  assert.ok(Math.abs(m.intercept - 1.0) < 1e-12);
  assert.strictEqual(mappingIsUncertain(m), false);
  assert.ok(Math.abs(api.programToSource(m, 0) - 1.0) < 1e-12);
  assert.ok(Math.abs(api.sourceToProgram(m, 1.0) - 0.0) < 1e-12);
});

check("unanchored source has no intervals and no sample (never file time)", () => {
  const clone = JSON.parse(JSON.stringify(project));
  clone.sources[0].anchors = [];
  assert.strictEqual(api.sourceProgramIntervals(clone.sources[0]), null);
  assert.strictEqual(api.sampleAt(clone.sources[0], 3.0), null);
});

check("uncertain anchors expose a comparable range wider than 40 ms", () => {
  const uncertain = JSON.parse(
    fs.readFileSync(path.join(__dirname, "..", "data", "projects", "news-uncertain.json"), "utf8")
  );
  const host = uncertain.sources.find((s) => s.id === "host");
  const m = api.fitMapping(host.anchors);
  assert.ok(rangeHalfWidth(m) > 0.04);
});

check("program intervals match clip layout with different offsets", () => {
  const guest = project.sources.find((s) => s.id === "guest");
  const intervals = api.sourceProgramIntervals(guest);
  // guest source -2..10, offset -2 -> program 0..12
  assert.strictEqual(intervals.length, 1);
  assert.ok(Math.abs(intervals[0][0] - 0.0) < 1e-9);
  assert.ok(Math.abs(intervals[0][1] - 12.0) < 1e-9);
});

check("sampleAt resolves the right clip and frame across a hole", () => {
  const host = project.sources.find((s) => s.id === "host");
  assert.ok(api.sampleAt(host, 5.0) !== null);
  // host hole program 11..14
  assert.strictEqual(api.sampleAt(host, 12.5), null);
  assert.ok(api.sampleAt(host, 15.0) !== null);
});

check("gapsIn reports host hole 11..14 and wide tail 28..30", () => {
  const host = project.sources.find((s) => s.id === "host");
  const hostGaps = api.gapsIn(host, 0, 30);
  assert.strictEqual(hostGaps.length, 1);
  assert.ok(Math.abs(hostGaps[0][0] - 11) < 1e-6 && Math.abs(hostGaps[0][1] - 14) < 1e-6);
  const wide = project.sources.find((s) => s.id === "wide");
  const wideGaps = api.gapsIn(wide, 0, 30);
  assert.strictEqual(wideGaps.length, 1);
  assert.ok(Math.abs(wideGaps[0][0] - 28) < 0.05 && Math.abs(wideGaps[0][1] - 30) < 1e-6);
});

check("visualChoiceAt returns black under black policy inside the hole", () => {
  api.load(JSON.parse(JSON.stringify(project)));
  const choice = api.visualChoiceAt(12.5);
  assert.strictEqual(choice.kind, "black");
});

check("fallback policy substitutes wide for the host hole", () => {
  const fb = JSON.parse(
    fs.readFileSync(path.join(__dirname, "..", "data", "projects", "news-fallback.json"), "utf8")
  );
  api.load(fb);
  const choice = api.visualChoiceAt(12.5);
  assert.strictEqual(choice.kind, "source");
  assert.strictEqual(choice.source.id, "wide");
  assert.strictEqual(choice.fallback, true);
  assert.strictEqual(choice.missing, "host");
});

check("damaged guest camera forces fallback/black, never a phantom frame", () => {
  const damaged = JSON.parse(
    fs.readFileSync(path.join(__dirname, "..", "data", "projects", "news-damaged.json"), "utf8")
  );
  api.load(damaged);
  // cut to guest spans 6..10.5; wide fallback covers it.
  const choice = api.visualChoiceAt(8.0);
  assert.strictEqual(choice.source.id, "wide");
  assert.strictEqual(choice.fallback, true);
});

check("rapid project keeps a valid source at every program frame", () => {
  const rapid = JSON.parse(
    fs.readFileSync(path.join(__dirname, "..", "data", "projects", "news-rapid.json"), "utf8")
  );
  api.load(rapid);
  let black = 0;
  for (let f = 0; f < 750; f++) {
    const t = f / 25;
    const c = api.visualChoiceAt(t);
    if (c.kind === "black") black += 1;
  }
  // Only the planned wide-tail black frames are allowed (25).
  assert.strictEqual(black, 25);
});

check("time formatting is stable and zero-padded", () => {
  assert.strictEqual(api.formatProgramTime(0), "00:00.000");
  assert.strictEqual(api.formatProgramTime(12.5), "00:12.500");
  const tc = api.formatTimecode(-2.0, 25, 1);
  assert.ok(tc.startsWith("-"));
});

check("frame grid keeps the last presentation frame and rejects pre-start", () => {
  // 25 fps clip [0,2): 50 frames; frame 49 owns [1.96,2.00).
  const clip = { path: "x.mp4", start: 0.0, duration: 2.0, frame_count: 50 };
  const source = { rate: { num: 25, den: 1 }, clips: [clip] };
  assert.strictEqual(api.clipFrameAt(clip, 1.96, source), 49);
  assert.strictEqual(api.clipFrameAt(clip, 1.999, source), 49);
  // At/after the clip end there is no presentation frame.
  assert.strictEqual(api.clipFrameAt(clip, 2.0, source), null);
  // A time before clip start is an ingest gap, never a clamped frame 0.
  assert.strictEqual(api.clipFrameAt(clip, -0.01, source), null);
  assert.strictEqual(api.clipFrameAt(clip, 0.0, source), 0);
});

check("switch token advances per cut so late results cannot replace identity", () => {
  const tokenA = api.nextSwitchToken("cam_a", 1.0);
  assert.strictEqual(api.currentSwitchToken(), tokenA);
  assert.strictEqual(api.getSwitchCache().sourceId, "cam_a");
  // A later cut starts a new, strictly greater generation.
  const tokenB = api.nextSwitchToken("cam_b", 1.5);
  assert.ok(tokenB > tokenA);

  // Simulate an in-flight seek from the old camera and a late decode result.
  const oldVideo = { el: { readyState: 1, currentTime: 0 } };
  api.seekSourceVideo(oldVideo, 1.0, tokenA);
  const newVideo = { el: { readyState: 2, currentTime: 0 } };
  api.seekSourceVideo(newVideo, 1.5, tokenB);

  const pctx = { fillStyle: "", fillRect: () => {}, drawImage: () => {} };
  // The stale camera-A result carrying the old token must be rejected.
  assert.strictEqual(api.drawFrameIfCurrent(pctx, oldVideo, "cam_a", tokenA, 640, 360), false);
  // Even a current-token frame is rejected when the camera id changed.
  assert.strictEqual(api.drawFrameIfCurrent(pctx, newVideo, "cam_a", tokenB, 640, 360), false);
  // The currently selected camera B with the current token is painted.
  assert.strictEqual(api.drawFrameIfCurrent(pctx, newVideo, "cam_b", tokenB, 640, 360), true);
});

console.log(`\n${passed} browser-logic checks passed`);
