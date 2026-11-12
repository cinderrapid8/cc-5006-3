"use strict";

// ---------------------------------------------------------------------------
// Pure timeline logic, mirrored from mcedit-core (Rust). Kept dependency-free
// so the browser preview makes exactly the same source/time decisions that the
// server-side render plan will make.
// ---------------------------------------------------------------------------

function fitMapping(anchors) {
  if (!anchors.length) return null;
  if (anchors.length === 1) {
    const a = anchors[0];
    return {
      slope: 1,
      intercept: a.source_time - a.program_time,
      fit_residual: 0,
      anchor_uncertainty: a.uncertainty || 0,
    };
  }
  const n = anchors.length;
  const meanP = anchors.reduce((s, a) => s + a.program_time, 0) / n;
  const meanS = anchors.reduce((s, a) => s + a.source_time, 0) / n;
  let varP = 0, cov = 0;
  for (const a of anchors) {
    const dp = a.program_time - meanP;
    cov += dp * (a.source_time - meanS);
    varP += dp * dp;
  }
  const slope = varP > 1e-12 ? cov / varP : 1;
  const intercept = meanS - slope * meanP;
  const residual = Math.sqrt(
    anchors.reduce((s, a) => {
      const e = slope * a.program_time + intercept - a.source_time;
      return s + e * e;
    }, 0) / n
  );
  return {
    slope,
    intercept,
    fit_residual: residual,
    anchor_uncertainty: Math.max(...anchors.map((a) => a.uncertainty || 0)),
  };
}

function rangeHalfWidth(m) {
  return m.fit_residual + m.anchor_uncertainty;
}

function mappingIsUncertain(m) {
  return rangeHalfWidth(m) > 0.04;
}

function programToSource(m, t) {
  return m.slope * t + m.intercept;
}

function sourceToProgram(m, t) {
  return (t - m.intercept) / m.slope;
}

function nearestFrame(tSec, rateNum, rateDen) {
  return Math.round((tSec * rateNum) / rateDen);
}

function mergedIntervals(intervals) {
  const sorted = [...intervals].sort((a, b) => a[0] - b[0]);
  const out = [];
  for (const span of sorted) {
    const last = out[out.length - 1];
    if (last && span[0] <= last[1] + 1e-6) last[1] = Math.max(last[1], span[1]);
    else out.push([span[0], span[1]]);
  }
  return out;
}

function sourceProgramIntervals(source) {
  const m = fitMapping(source.anchors);
  if (!m) return null;
  const spans = source.clips.map((c) => {
    const p0 = sourceToProgram(m, c.start);
    const p1 = sourceToProgram(m, c.start + c.duration);
    return [Math.min(p0, p1), Math.max(p0, p1)];
  });
  return mergedIntervals(spans);
}

function effectiveFrameCount(clip, source) {
  // The manifest carries an explicit presentation frame count; fall back to the
  // duration-derived grid when it is missing.
  if (clip.frame_count && clip.frame_count > 0) return clip.frame_count;
  return Math.max(0, Math.round((clip.duration * source.rate.num) / source.rate.den));
}

// Presentation frame index covering sourceTime inside a clip, or null. Frame k
// is presented over [start + k*frame_dur, start + (k+1)*frame_dur), so the
// clip's last frame owns the slice up to the clip end. Decode order (B-frames)
// never enters this decision.
function clipFrameAt(clip, sourceTime, source) {
  const frameDur = source.rate.den / source.rate.num;
  const count = effectiveFrameCount(clip, source);
  if (count <= 0) return null;
  if (sourceTime < clip.start) return null;
  const rel = sourceTime - clip.start;
  const idx = Math.max(0, Math.floor(rel / frameDur));
  return idx < count ? idx : null;
}

function sampleAt(source, programTime) {
  const m = fitMapping(source.anchors);
  if (!m) return null;
  const sourceTime = programToSource(m, programTime);
  // Check clips by ascending source start so an ingest boundary resolves to the
  // earlier clip deterministically; this is a local decision, never a global
  // shift of frame numbers.
  const ordered = [...source.clips].map((clip, original) => ({ clip, original }))
    .sort((a, b) => (a.clip.start - b.clip.start) || (a.original - b.original));
  for (const { clip } of ordered) {
    const frameIndex = clipFrameAt(clip, sourceTime, source);
    if (frameIndex !== null) {
      const clipOffset = Math.max(0, sourceTime - clip.start);
      return { sourceTime, clip, clipOffset, frameIndex };
    }
  }
  return null;
}

function gapsIn(source, start, end) {
  const intervals = sourceProgramIntervals(source);
  if (!intervals) return [[start, end]];
  const gaps = [];
  let cursor = start;
  for (const [a, b] of intervals) {
    if (b <= cursor) continue;
    if (a >= end) break;
    if (a > cursor) gaps.push([cursor, Math.min(a, end)]);
    cursor = Math.max(cursor, b);
    if (cursor >= end) break;
  }
  if (cursor < end) gaps.push([cursor, end]);
  return gaps;
}

function formatProgramTime(t) {
  const v = Math.max(0, t);
  const m = Math.floor(v / 60);
  const s = v % 60;
  return `${String(m).padStart(2, "0")}:${s.toFixed(3).padStart(6, "0")}`;
}

function formatTimecode(tSec, rateNum, rateDen) {
  const neg = tSec < 0;
  const fps = Math.round(rateNum / rateDen) || 25;
  const total = nearestFrame(Math.abs(tSec), rateNum, rateDen);
  const frames = ((total % fps) + fps) % fps;
  const totalSec = Math.floor(total / fps);
  const sec = ((totalSec % 60) + 60) % 60;
  const min = Math.floor(totalSec / 60) % 60;
  const hr = Math.floor(totalSec / 3600);
  return `${neg ? "-" : ""}${String(hr).padStart(2, "0")}:${String(min).padStart(2, "0")}:${String(
    sec
  ).padStart(2, "0")}:${String(frames).padStart(2, "0")}`;
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

const els = {};
[
  "project-select", "btn-load", "btn-new", "btn-validate", "btn-save", "btn-render",
  "preview", "preview-badge", "gap-banner", "btn-play", "btn-pause", "btn-cut",
  "clock", "output-frame", "scrub", "duration-label", "cuts-table", "btn-clear-cuts",
  "sources", "fallback-source", "audio-source", "audio-start", "report", "render-status",
  "anchor-dialog", "anchor-form", "anchor-source-name", "anchor-label", "anchor-program",
  "anchor-source", "anchor-uncertainty", "anchor-range", "new-dialog", "new-form",
  "new-name", "new-duration", "new-rate", "monitors",
].forEach((id) => (els[id] = document.getElementById(id)));
// Camel-case aliases used by the playback wiring.
Object.assign(els, {
  btnPlay: els["btn-play"],
  btnPause: els["btn-pause"],
  btnCut: els["btn-cut"],
  btnLoad: els["btn-load"],
  btnNew: els["btn-new"],
  btnValidate: els["btn-validate"],
  btnSave: els["btn-save"],
  btnRender: els["btn-render"],
  btnClearCuts: els["btn-clear-cuts"],
});

let project = null;
let playing = false;
let programTime = 0;
let rafId = null;
let lastWall = 0;
let activeVideo = null; // video element driving the clock
const videos = new Map(); // sourceId -> {el, canvas, ctx, monitor}
let editingAnchorSource = null;

const DEFAULT_PROJECT_NAME = "untitled";

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

async function api(path, options = {}) {
  const resp = await fetch(path, {
    headers: { "Content-Type": "application/json" },
    ...options,
  });
  const text = await resp.text();
  const data = text ? JSON.parse(text) : {};
  if (!resp.ok) throw new Error(data.error || `HTTP ${resp.status}`);
  return data;
}

function mediaUrl(clipPath) {
  return "/raw/" + clipPath;
}

// ---------------------------------------------------------------------------
// Project lifecycle
// ---------------------------------------------------------------------------

function blankProject(name, duration, rateText) {
  const [num, den] = rateText.includes("/")
    ? rateText.split("/").map(Number)
    : [Number(rateText), 1];
  return {
    name,
    output: { width: 640, height: 360, rate: { num, den }, sample_rate: 48000, duration },
    sources: [],
    cuts: [],
    gap_policy: { mode: "black" },
    audio: { source_id: "", program_start: 0, external_path: null },
  };
}

async function refreshProjectList() {
  // The std server has no list endpoint; keep the select populated from
  // localStorage history plus whatever is currently loaded.
  const known = JSON.parse(localStorage.getItem("mcedit-projects") || "[]");
  if (project && !known.includes(project.name)) known.push(project.name);
  localStorage.setItem("mcedit-projects", JSON.stringify(known));
  els["project-select"].innerHTML = known
    .map((n) => `<option ${project && project.name === n ? "selected" : ""}>${n}</option>`)
    .join("");
}

async function loadProjectByName(name) {
  project = await api(`/api/projects/${encodeURIComponent(name)}`);
  stopPlayback();
  programTime = 0;
  buildMonitors();
  renderAll();
  refreshProjectList();
}

// ---------------------------------------------------------------------------
// Monitors: one <video> per clip, drawn onto per-source canvases.
// ---------------------------------------------------------------------------

function buildMonitors() {
  els.monitors.innerHTML = "";
  videos.clear();
  for (const source of project.sources) {
    const monitor = document.createElement("div");
    monitor.className = "monitor";
    monitor.dataset.source = source.id;
    monitor.innerHTML = `
      <canvas width="320" height="180"></canvas>
      <div class="mon-label">${source.name}</div>
      <div class="mon-frame">--</div>`;
    els.monitors.appendChild(monitor);
    const canvas = monitor.querySelector("canvas");
    const el = document.createElement("video");
    el.muted = true;
    el.preload = "auto";
    el.playsInline = true;
    el.className = "source-video";
    const firstClip = source.clips[0];
    if (firstClip) el.src = mediaUrl(firstClip.path);
    monitor.appendChild(el);
    videos.set(source.id, {
      el,
      canvas,
      ctx: canvas.getContext("2d"),
      monitor,
      loadedClip: firstClip ? firstClip.path : null,
    });
  }
}

function ensureClipLoaded(source, clipPath) {
  const v = videos.get(source.id);
  if (!v || v.loadedClip === clipPath) return;
  v.el.src = mediaUrl(clipPath);
  v.loadedClip = clipPath;
}

// ---------------------------------------------------------------------------
// Preview: same visual decision the Rust RenderPlan makes per frame.
// ---------------------------------------------------------------------------

function currentCutIndex(t) {
  let idx = -1;
  for (let i = 0; i < project.cuts.length; i++) {
    if (project.cuts[i].start <= t + 1e-9) idx = i;
    else break;
  }
  return idx;
}

function visualChoiceAt(t) {
  const idx = currentCutIndex(t);
  if (idx < 0) return { kind: "black" };
  const cut = project.cuts[idx];
  const source = project.sources.find((s) => s.id === cut.source_id);
  if (source) {
    const sample = sampleAt(source, t);
    if (sample) return { kind: "source", source, sample, fallback: false };
  }
  if (project.gap_policy.mode === "fallback") {
    const fb = project.sources.find((s) => s.id === project.gap_policy.source_id);
    if (fb) {
      const sample = sampleAt(fb, t);
      if (sample) return { kind: "source", source: fb, sample, fallback: true, missing: cut.source_id };
    }
  }
  return { kind: "black", missing: cut ? cut.source_id : null };
}

function drawCover(ctx, video, w, h) {
  const vw = video.videoWidth || w;
  const vh = video.videoHeight || h;
  const scale = Math.max(w / vw, h / vh);
  const dw = vw * scale;
  const dh = vh * scale;
  ctx.drawImage(video, (w - dw) / 2, (h - dh) / 2, dw, dh);
}

function syncSourceVideo(source, t) {
  const v = videos.get(source.id);
  if (!v) return;
  const sample = sampleAt(source, t);
  if (!sample) {
    return;
  }
  ensureClipLoaded(source, sample.clip.path);
  const target = sample.clipOffset;
  const el = v.el;
  if (!el.getClientRects || el.readyState < 1) return;
  // Parked (non-master) monitors are never the program output, so they use
  // their own per-element token, distinct from the program switch token.
  if (v.parkToken === undefined) v.parkToken = 0;
  if (Math.abs(el.currentTime - target) > 0.12) {
    v.parkToken += 1;
    el.currentTime = target;
  }
}

let lastActiveSourceId = null;

// ---------------------------------------------------------------------------
// Switch cache. Every program-time selection gets a monotonically increasing
// switch token; seeks and frame draws carry the token and the selected camera
// identity. A late result from an earlier switch (B-frame decode reordering or
// an async seek resolving after the cut) is discarded, so the camera already
// chosen for the current engineering time can never be replaced by a stale
// frame from the previous camera.
// ---------------------------------------------------------------------------

let switchSeq = 0;
const switchCache = {
  token: 0,
  // camera id selected at the cached program position; null means black
  sourceId: null,
  programTime: 0,
};

function nextSwitchToken(sourceId, t) {
  switchSeq += 1;
  switchCache.token = switchSeq;
  switchCache.sourceId = sourceId;
  switchCache.programTime = t;
  return switchCache.token;
}

function currentSwitchToken() {
  return switchCache.token;
}

// Tag a seek with the switch generation that requested it. Seeks issued by an
// older generation are overwritten rather than allowed to land late.
function seekSourceVideo(v, target, token) {
  v.switchToken = token;
  if (v.el.readyState < 1) return token;
  if (v.switchToken === token && Math.abs(v.el.currentTime - target) <= 0.12) {
    return token;
  }
  v.el.currentTime = target;
  return token;
}

// Paint only when the frame still belongs to the selected camera and current
// switch generation. Late frames from a previous cut are rejected here.
function drawFrameIfCurrent(pctx, v, sourceId, token, W, H) {
  if (v.switchToken !== token || switchCache.sourceId !== sourceId) return false;
  if (v.el.readyState < 2) return false;
  pctx.fillStyle = "#000";
  pctx.fillRect(0, 0, W, H);
  drawCover(pctx, v.el, W, H);
  return true;
}

function updatePreview(t) {
  const pctx = els.preview.getContext("2d");
  const W = els.preview.width;
  const H = els.preview.height;
  const choice = visualChoiceAt(t);
  const chosenId = choice.kind === "source" ? choice.source.id : null;
  // A different camera identity (or a move to black) starts a new switch
  // generation; large scrub jumps likewise invalidate in-flight seeks.
  if (chosenId !== switchCache.sourceId || Math.abs(t - switchCache.programTime) > 0.5) {
    nextSwitchToken(chosenId, t);
  }
  const token = currentSwitchToken();

  if (choice.kind === "black") {
    pctx.fillStyle = "#000";
    pctx.fillRect(0, 0, W, H);
    els["preview-badge"].textContent = "黑场（源无数据）";
    els["gap-banner"].classList.remove("hidden");
    els["gap-banner"].textContent = choice.missing
      ? `缺口：机位 ${choice.missing} 在 ${formatProgramTime(t)} 无记录，按策略留黑`
      : "节目时间无切镜，输出黑场";
  } else {
    const { source, sample, fallback, missing } = choice;
    const v = videos.get(source.id);
    ensureClipLoaded(source, sample.clip.path);
    let painted = false;
    if (v) {
      seekSourceVideo(v, sample.clipOffset, token);
      painted = drawFrameIfCurrent(pctx, v, source.id, token, W, H);
    }
    if (!painted) {
      // No current frame yet (seek/decode still in flight): hold a neutral
      // slate, never the previous camera's late frame.
      pctx.fillStyle = "#111";
      pctx.fillRect(0, 0, W, H);
    }
    const fbText = fallback ? `（备用替代 ${missing}）` : "";
    els["preview-badge"].textContent =
      `${source.name} · 源帧 ${sample.frameIndex} · ${formatTimecode(
        sample.sourceTime, source.rate.num, source.rate.den
      )}${fbText}`;
    if (fallback) {
      els["gap-banner"].classList.remove("hidden");
      els["gap-banner"].textContent =
        `缺口：机位 ${missing} 在 ${formatProgramTime(t)} 无记录，已退回备用机位 ${source.name}`;
    } else {
      els["gap-banner"].classList.add("hidden");
    }
  }

  // Per-source monitors: availability + actual current frame.
  for (const source of project.sources) {
    const v = videos.get(source.id);
    if (!v) continue;
    const sample = sampleAt(source, t);
    v.monitor.classList.toggle("gap", !sample);
    v.monitor.classList.toggle(
      "active",
      choice.kind === "source" && choice.source.id === source.id
    );
    const frameLabel = v.monitor.querySelector(".mon-frame");
    if (sample) {
      frameLabel.textContent = `f${sample.frameIndex}`;
      ensureClipLoaded(source, sample.clip.path);
      if (v.el.readyState >= 2) drawCover(v.ctx, v.el, v.canvas.width, v.canvas.height);
      else {
        v.ctx.fillStyle = "#111";
        v.ctx.fillRect(0, 0, v.canvas.width, v.canvas.height);
      }
    } else {
      frameLabel.textContent = "缺口";
      v.ctx.fillStyle = "#000";
      v.ctx.fillRect(0, 0, v.canvas.width, v.canvas.height);
      v.ctx.fillStyle = "#c0502e";
      v.ctx.font = "bold 18px sans-serif";
      v.ctx.fillText("NO SIGNAL", 18, v.canvas.height / 2);
    }
  }

  els.clock.textContent = formatProgramTime(t);
  const rate = project.output.rate;
  els["output-frame"].textContent = String(nearestFrame(t, rate.num, rate.den));
  els.scrub.value = String(Math.round((t / project.output.duration) * 1000));

  const activeId = choice.kind === "source" ? choice.source.id : null;
  if (playing && activeId !== lastActiveSourceId) {
    switchClockMaster(choice);
    lastActiveSourceId = activeId;
  }
}

// ---------------------------------------------------------------------------
// Playback clock. The video element of the currently selected source is the
// master clock; switching sources swaps master and hard-seeks. Audio is only
// ever unmuted on the chosen audio master's element.
// ---------------------------------------------------------------------------

function switchClockMaster(choice) {
  // A master swap invalidates every in-flight seek so a late frame from the
  // previous camera cannot be painted after the cut.
  const newId = choice.kind === "source" ? choice.source.id : null;
  if (newId !== switchCache.sourceId) {
    nextSwitchToken(newId, switchCache.programTime);
  }
  const token = currentSwitchToken();
  for (const [id, v] of videos) {
    v.el.pause();
    v.el.muted = true;
    v.switchToken = -1;
  }
  activeVideo = null;
  if (choice.kind !== "source") return;
  const v = videos.get(choice.source.id);
  if (!v) return;
  seekSourceVideo(v, choice.sample.clipOffset, token);
  v.el.muted = project.audio.source_id !== choice.source.id || !!project.audio.external_path;
  v.el.playbackRate = 1;
  const p = v.el.play();
  if (p) p.catch(() => {});
  activeVideo = v.el;
}

function tick(now) {
  if (!playing || !project) return;
  const wallDelta = (now - lastWall) / 1000;
  lastWall = now;

  if (activeVideo && activeVideo.readyState >= 2 && !activeVideo.paused) {
    // Derive program time from the master video's clip position.
    const choice = visualChoiceAt(programTime);
    if (choice.kind === "source") {
      const sample = sampleAt(choice.source, programTime);
      if (sample) {
        const drift = activeVideo.currentTime - sample.clipOffset;
        if (Math.abs(drift) > 0.2) activeVideo.currentTime = sample.clipOffset;
      }
    }
    programTime += wallDelta;
  } else {
    programTime += wallDelta;
  }

  if (programTime >= project.output.duration) {
    programTime = project.output.duration;
    stopPlayback();
    updatePreview(programTime);
    return;
  }

  // Keep every non-master element parked at the frame its source would show.
  const choice = visualChoiceAt(programTime);
  for (const source of project.sources) {
    if (choice.kind === "source" && source.id === choice.source.id) continue;
    syncSourceVideo(source, programTime);
  }
  updatePreview(programTime);
  rafId = requestAnimationFrame(tick);
}

function startPlayback() {
  if (!project || playing) return;
  if (programTime >= project.output.duration - 1e-6) programTime = 0;
  playing = true;
  lastWall = performance.now();
  lastActiveSourceId = null;
  updatePreview(programTime);
  rafId = requestAnimationFrame(tick);
}

function stopPlayback() {
  playing = false;
  if (rafId) cancelAnimationFrame(rafId);
  rafId = null;
  for (const v of videos.values()) {
    v.el.pause();
  }
  activeVideo = null;
}

// ---------------------------------------------------------------------------
// Cuts
// ---------------------------------------------------------------------------

function addCut() {
  if (!project) return;
  const choice = visualChoiceAt(programTime);
  const sourceId = choice.kind === "source" ? choice.source.id : null;
  if (!sourceId) {
    alert("当前节目位置没有可用机位画面，无法在此切镜（请先移动到有数据的区间或设置锚点）。");
    return;
  }
  // Replace an existing cut at the same instant instead of stacking duplicates.
  const existing = project.cuts.find((c) => Math.abs(c.start - programTime) < 1e-6);
  if (existing) {
    existing.source_id = sourceId;
  } else {
    project.cuts.push({
      id: `cut-${project.cuts.length + 1}`,
      start: programTime,
      source_id: sourceId,
    });
    project.cuts.sort((a, b) => a.start - b.start);
    project.cuts.forEach((c, i) => (c.id = `cut-${i + 1}`));
  }
  renderCuts();
  updatePreview(programTime);
}

function removeCut(id) {
  project.cuts = project.cuts.filter((c) => c.id !== id);
  project.cuts.forEach((c, i) => (c.id = `cut-${i + 1}`));
  renderCuts();
}

function gapForCut(cut, index) {
  const end = project.cuts[index + 1]?.start ?? project.output.duration;
  const source = project.sources.find((s) => s.id === cut.source_id);
  if (!source) return [[cut.start, end]];
  return gapsIn(source, cut.start, end).filter(([a, b]) => b - a > 1e-6);
}

function renderCuts() {
  const tbody = els["cuts-table"].querySelector("tbody");
  tbody.innerHTML = project.cuts
    .map((cut, i) => {
      const gaps = gapForCut(cut, i);
      const source = project.sources.find((s) => s.id === cut.source_id);
      const gapText = gaps.length
        ? gaps.map(([a, b]) => `${formatProgramTime(a)}–${formatProgramTime(b)}`).join("<br>")
        : "无";
      return `<tr>
        <td>${i + 1}</td>
        <td>${formatProgramTime(cut.start)}</td>
        <td>${source ? source.name : cut.source_id}</td>
        <td class="gap-cell ${gaps.length ? "" : "none"}">${gapText}</td>
        <td><button class="danger" data-del="${cut.id}">删除</button></td>
      </tr>`;
    })
    .join("");
  tbody.querySelectorAll("[data-del]").forEach((btn) =>
    btn.addEventListener("click", () => removeCut(btn.dataset.del))
  );
}

// ---------------------------------------------------------------------------
// Sources panel: anchors, intervals, current frame, comparable range
// ---------------------------------------------------------------------------

function renderSources() {
  const root = els.sources;
  root.innerHTML = "";
  els["fallback-source"].innerHTML = project.sources
    .map((s) => `<option value="${s.id}">${s.name}</option>`)
    .join("");
  els["audio-source"].innerHTML =
    `<option value="">（外部文件）</option>` +
    project.sources.map((s) => `<option value="${s.id}">${s.name} 机内音</option>`).join("");
  els["audio-source"].value = project.audio.source_id;
  els["audio-start"].value = project.audio.program_start;
  document.querySelector('input[name="gap"][value="black"]').checked =
    project.gap_policy.mode === "black";
  document.querySelector('input[name="gap"][value="fallback"]').checked =
    project.gap_policy.mode === "fallback";
  if (project.gap_policy.mode === "fallback") {
    els["fallback-source"].value = project.gap_policy.source_id;
  }

  for (const source of project.sources) {
    const mapping = fitMapping(source.anchors);
    const intervals = sourceProgramIntervals(source);
    const card = document.createElement("div");
    card.className =
      "source-card " + (!mapping ? "unanchored" : mappingIsUncertain(mapping) ? "uncertain" : "");

    const tag = !mapping
      ? '<span class="tag bad">未对齐</span>'
      : mappingIsUncertain(mapping)
      ? '<span class="tag warn">锚点不确定</span>'
      : '<span class="tag ok">已对齐</span>';

    const barSpans = intervals
      ? intervals
          .map(([a, b]) => {
            const left = (Math.max(0, a) / project.output.duration) * 100;
            const width = ((b - a) / project.output.duration) * 100;
            return `<span class="avail" style="left:${left}%;width:${width}%"></span>`;
          })
          .join("")
      : "";

    const intervalText = intervals
      ? intervals.map(([a, b]) => `${formatProgramTime(a)}–${formatProgramTime(b)}`).join("，")
      : "（需要至少一个人工锚点）";

    const sample = mapping ? sampleAt(source, programTime) : null;
    const currentFrame = sample
      ? `源帧 ${sample.frameIndex} · ${formatTimecode(sample.sourceTime, source.rate.num, source.rate.den)}`
      : mapping
      ? "当前节目位置：缺口"
      : "—";

    let rangeHtml = "";
    if (mapping && mappingIsUncertain(mapping)) {
      const w = rangeHalfWidth(mapping) * Math.abs(mapping.slope);
      const [lo, hi] = [programTime - w, programTime + w];
      rangeHtml = `<div class="range-box">锚点不确定：节目 ${formatProgramTime(
        programTime
      )} 对应的可比范围 ${formatProgramTime(lo)} – ${formatProgramTime(hi)}（±${w.toFixed(3)}s）</div>`;
    }

    const anchorRows = source.anchors
      .map(
        (a, i) => `<div class="anchor-row">
          <span>${a.label || "锚点 " + (i + 1)}：节目 ${a.program_time.toFixed(3)}s ↔ 本机 ${a.source_time.toFixed(3)}s${
          a.uncertainty ? `（±${a.uncertainty}s）` : ""
        }</span>
          <button data-rm-anchor="${source.id}:${i}" class="danger">删</button>
        </div>`
      )
      .join("");

    card.innerHTML = `
      <div class="source-head">
        <b>${source.name}</b> ${tag}
      </div>
      <div class="mapping-row">
        <span>角色 ${source.role || "—"} · 帧率 ${source.rate.num}/${source.rate.den}</span>
        <span>${currentFrame}</span>
      </div>
      <div class="bar">
        ${barSpans}
        <span class="cursor" style="left:${(programTime / project.output.duration) * 100}%"></span>
      </div>
      <div class="interval"><span>可用区间（节目时间）</span><span>${intervalText}</span></div>
      ${rangeHtml}
      <div class="anchor-list">${anchorRows}</div>
      <div style="margin-top:8px; display:flex; gap:6px;">
        <button data-anchor="${source.id}">添加/编辑锚点</button>
      </div>`;
    root.appendChild(card);
  }

  root.querySelectorAll("[data-anchor]").forEach((btn) =>
    btn.addEventListener("click", () => openAnchorDialog(btn.dataset.anchor))
  );
  root.querySelectorAll("[data-rm-anchor]").forEach((btn) =>
    btn.addEventListener("click", () => {
      const [sid, idx] = btn.dataset.rmAnchor.split(":");
      const s = project.sources.find((x) => x.id === sid);
      s.anchors.splice(Number(idx), 1);
      renderAll();
    })
  );
}

function openAnchorDialog(sourceId) {
  const source = project.sources.find((s) => s.id === sourceId);
  editingAnchorSource = sourceId;
  els["anchor-source-name"].textContent = source.name;
  els["anchor-label"].value = "";
  els["anchor-program"].value = programTime.toFixed(3);
  const sample = sampleAt(source, programTime);
  els["anchor-source"].value = sample ? sample.sourceTime.toFixed(3) : "0";
  els["anchor-uncertainty"].value = "0";
  els["anchor-range"].textContent = "";
  els["anchor-dialog"].showModal();
}

els["anchor-form"].addEventListener("input", () => {
  const p = Number(els["anchor-program"].value);
  const s = Number(els["anchor-source"].value);
  const u = Number(els["anchor-uncertainty"].value) || 0;
  const source = project.sources.find((x) => x.id === editingAnchorSource);
  const virtualAnchors = [...source.anchors, { program_time: p, source_time: s, uncertainty: u }];
  const m = fitMapping(virtualAnchors);
  if (m && mappingIsUncertain(m)) {
    const w = rangeHalfWidth(m) * Math.abs(m.slope);
    els["anchor-range"].textContent =
      `该组锚点将产生 ±${w.toFixed(3)}s 的可比范围（超过 40ms 即标记为不确定）`;
  } else {
    els["anchor-range"].textContent = "映射在精确范围内。";
  }
});

els["anchor-form"].addEventListener("submit", (e) => {
  if (e.submitter && e.submitter.value !== "save") return;
  const source = project.sources.find((x) => x.id === editingAnchorSource);
  source.anchors.push({
    id: `anc-${source.anchors.length + 1}-${Date.now()}`,
    label: els["anchor-label"].value || `锚点 ${source.anchors.length + 1}`,
    program_time: Number(els["anchor-program"].value),
    source_time: Number(els["anchor-source"].value),
    uncertainty: Number(els["anchor-uncertainty"].value) || 0,
  });
  renderAll();
});

// ---------------------------------------------------------------------------
// Gap policy / audio master
// ---------------------------------------------------------------------------

document.querySelectorAll('input[name="gap"]').forEach((radio) =>
  radio.addEventListener("change", () => {
    if (radio.value === "black") project.gap_policy = { mode: "black" };
    else
      project.gap_policy = {
        mode: "fallback",
        source_id: els["fallback-source"].value,
      };
  })
);
els["fallback-source"].addEventListener("change", () => {
  if (project.gap_policy.mode === "fallback")
    project.gap_policy.source_id = els["fallback-source"].value;
});
els["audio-source"].addEventListener("change", () => {
  project.audio.source_id = els["audio-source"].value;
});
els["audio-start"].addEventListener("change", () => {
  project.audio.program_start = Number(els["audio-start"].value) || 0;
});

// ---------------------------------------------------------------------------
// Validation, saving, rendering
// ---------------------------------------------------------------------------

function validateLocally() {
  const errors = [], warnings = [], gaps = [];
  if (project.output.duration <= 0) errors.push("输出时长必须为正");
  if (!project.cuts.length) errors.push("时间线没有任何切镜点");
  for (const s of project.sources) {
    const m = fitMapping(s.anchors);
    if (!m) warnings.push(`机位 ${s.name} 没有人工锚点，无法对齐`);
    else if (mappingIsUncertain(m))
      warnings.push(`机位 ${s.name} 锚点不确定，界面显示可比范围 ±${rangeHalfWidth(m).toFixed(3)}s`);
  }
  project.cuts.forEach((cut, i) => {
    const source = project.sources.find((s) => s.id === cut.source_id);
    if (!source) {
      errors.push(`切镜 ${cut.id} 引用了不存在的机位`);
      return;
    }
    const end = project.cuts[i + 1]?.start ?? project.output.duration;
    for (const [a, b] of gapsIn(source, cut.start, end)) {
      if (b - a > 1e-6) gaps.push({ cut_id: cut.id, source_id: cut.source_id, start: a, end: b });
    }
  });
  if (project.gap_policy.mode === "fallback") {
    const fb = project.sources.find((s) => s.id === project.gap_policy.source_id);
    if (!fb) errors.push("备用机位不存在");
    else if (!fitMapping(fb.anchors)) errors.push("备用机位没有锚点");
  }
  if (!project.audio.source_id && !project.audio.external_path)
    errors.push("必须指定音频主轨机位或外部文件");
  return { ok: errors.length === 0, errors, warnings, gaps };
}

async function validateAndShow() {
  const local = validateLocally();
  // Server-side validation is the authoritative check (same code as render).
  let server = null;
  try {
    await api(`/api/projects/${encodeURIComponent(project.name)}`, {
      method: "POST",
      body: JSON.stringify(project),
    });
    server = await api(`/api/projects/${encodeURIComponent(project.name)}/validate`, {
      method: "POST",
    });
  } catch (e) {
    server = { ok: false, errors: [e.message], warnings: [], gaps: [] };
  }
  const report = server.ok && !local.errors.length ? server : mergeReports(local, server);
  renderReport(report);
  return report;
}

function mergeReports(local, server) {
  return {
    ok: local.ok && server.ok,
    errors: [...new Set([...local.errors, ...(server.errors || [])])],
    warnings: [...new Set([...local.warnings, ...(server.warnings || [])])],
    gaps: server.gaps || local.gaps,
  };
}

function renderReport(report) {
  const parts = [];
  parts.push(
    `<div class="${report.ok ? "ok" : "err"}">${report.ok ? "校验通过，可以导出" : "校验未通过"}</div>`
  );
  for (const e of report.errors) parts.push(`<div class="err">✗ ${e}</div>`);
  for (const w of report.warnings) parts.push(`<div class="warn">⚠ ${w}</div>`);
  if (report.gaps?.length) {
    parts.push(`<div class="warn">检测到 ${report.gaps.length} 个数据缺口：</div>`);
    for (const g of report.gaps)
      parts.push(
        `<div class="warn">· 切镜 ${g.cut_id} / 机位 ${g.source_id}：${formatProgramTime(
          g.start
        )}–${formatProgramTime(g.end)}（${(g.end - g.start).toFixed(3)}s）</div>`
      );
  }
  els.report.innerHTML = parts.join("");
}

async function saveProject() {
  if (!project.name) project.name = DEFAULT_PROJECT_NAME;
  await api("/api/projects", { method: "POST", body: JSON.stringify(project) });
  await refreshProjectList();
  els.report.innerHTML = `<div class="ok">工程已保存，修订号 ${await revisionHint()}</div>`;
}

async function revisionHint() {
  // Revision is computed server-side; fetch it through validate response echo.
  const data = await api(`/api/projects/${encodeURIComponent(project.name)}/validate`, {
    method: "POST",
  });
  return data.revision || "(见导出结果)";
}

async function renderProject() {
  const report = await validateAndShow();
  if (!report.ok) {
    alert("校验未通过，不能导出。请先处理错误与缺口策略。");
    return;
  }
  els["render-status"].innerHTML = `<div class="warn">渲染任务已提交…</div>`;
  const job = await api(`/api/projects/${encodeURIComponent(project.name)}/render`, {
    method: "POST",
  });
  pollJob(job.job_id);
}

async function pollJob(jobId) {
  const job = await api(`/api/jobs/${encodeURIComponent(jobId)}`);
  if (job.status === "running") {
    setTimeout(() => pollJob(jobId), 700);
    return;
  }
  if (job.status === "error") {
    els["render-status"].innerHTML = `<div class="err">渲染失败：${job.message}</div>`;
    return;
  }
  const r = job.result;
  els["render-status"].innerHTML = `
    <div class="ok">导出完成 · 固定剪辑修订 <b>${r.revision}</b></div>
    <div class="hint">视频解码哈希 ${r.video_hash.slice(0, 16)}… · 音频解码哈希 ${r.audio_hash.slice(0, 16)}…</div>
    <div><a href="${r.review_url}" target="_blank">审看 MP4</a> ·
         <a href="${r.master_url}" target="_blank">无损母版 MKV</a> ·
         <a href="${r.plan_url}" target="_blank">帧级计划</a> ·
         <a href="${r.sidecar_url}" target="_blank">侧车清单</a></div>`;
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

function renderAll() {
  if (!project) return;
  els["duration-label"].textContent = formatProgramTime(project.output.duration);
  renderSources();
  renderCuts();
  updatePreview(programTime);
}

els.btnPlay.addEventListener("click", startPlayback);
els.btnPause.addEventListener("click", () => {
  stopPlayback();
  updatePreview(programTime);
});
els.btnCut.addEventListener("click", addCut);
els.btnClearCuts.addEventListener("click", () => {
  if (project && confirm("清空全部切镜点？")) {
    project.cuts = [];
    renderCuts();
  }
});
els.scrub.addEventListener("input", () => {
  stopPlayback();
  programTime = (Number(els.scrub.value) / 1000) * project.output.duration;
  updatePreview(programTime);
});
els.btnLoad.addEventListener("click", () =>
  loadProjectByName(els["project-select"].value).catch((e) => alert(e.message))
);
els.btnValidate.addEventListener("click", () => validateAndShow().catch((e) => alert(e.message)));
els.btnSave.addEventListener("click", () => saveProject().catch((e) => alert(e.message)));
els.btnRender.addEventListener("click", () => renderProject().catch((e) => alert(e.message)));

els.btnNew.addEventListener("click", () => els["new-dialog"].showModal());
els["new-form"].addEventListener("submit", (e) => {
  if (e.submitter?.value !== "create") return;
  project = blankProject(
    els["new-name"].value || "untitled",
    Number(els["new-duration"].value) || 30,
    els["new-rate"].value || "25"
  );
  stopPlayback();
  programTime = 0;
  buildMonitors();
  renderAll();
  refreshProjectList();
});

// Expose a tiny test/debug surface for headless checks.
window.mcedit = {
  load: (p) => {
    project = p;
    buildMonitors();
    renderAll();
  },
  get project() {
    return project;
  },
  sampleAt,
  gapsIn,
  fitMapping,
  mappingIsUncertain,
  rangeHalfWidth,
  programToSource,
  sourceToProgram,
  visualChoiceAt,
  sourceProgramIntervals,
  formatProgramTime,
  formatTimecode,
  nearestFrame,
  clipFrameAt,
  effectiveFrameCount,
  nextSwitchToken,
  currentSwitchToken,
  seekSourceVideo,
  drawFrameIfCurrent,
  // Read-only view of the switch generation cache for headless regression
  // tests of the late-result identity guard.
  getSwitchCache: () => ({ ...switchCache }),
};

refreshProjectList();
