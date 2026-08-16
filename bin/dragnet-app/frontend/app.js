// SPDX-License-Identifier: AGPL-3.0-only
"use strict";

const invoke = (cmd, args) => {
  const t = window.__TAURI__;
  if (!t || !t.core) return Promise.reject(new Error("Arayüz hazır değil"));
  return t.core.invoke(cmd, args || {});
};
const $ = (id) => document.getElementById(id);

const CAT = {
  video: "Film/Video", audio: "Müzik", software: "Yazılım", game: "Oyun",
  book: "Kitap", adult: "Yetişkin", archive: "Arşiv", other: "Diğer",
};
function catLabel(c) { return CAT[c] || "Diğer"; }
function catChip(c) { const cls = CAT[c] ? c : "other"; return `<span class="chip ${cls}">${catLabel(c)}</span>`; }

function humanSize(bytes) {
  const u = ["B", "KB", "MB", "GB", "TB", "PB"];
  let n = Number(bytes) || 0, i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(n < 10 && i > 0 ? 1 : 0)} ${u[i]}`;
}
function nf(n) { return (Number(n) || 0).toLocaleString("tr"); }
function esc(s) { return String(s).replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c])); }
function escAttr(s) { return esc(s).replace(/"/g, "&quot;"); }
function dateShort(ts) { if (!ts) return "–"; return new Date(ts * 1000).toLocaleDateString("tr", { day: "2-digit", month: "2-digit", year: "2-digit" }); }
function dateFull(ts) { return ts ? new Date(ts * 1000).toLocaleString("tr") : ""; }

function toast(msg) {
  const el = $("toast"); el.textContent = msg; el.classList.remove("hidden");
  clearTimeout(toast._t); toast._t = setTimeout(() => el.classList.add("hidden"), 2600);
}
function copyMagnet(m) { navigator.clipboard.writeText(m).then(() => toast("Magnet kopyalandı"), () => toast("Kopyalanamadı")); }

function peerCell(p) {
  if (p == null || p < 0) return `<span class="muted" title="Henüz kontrol edilmedi">–</span>`;
  if (p === 0) return `<span class="dead" title="Canlı peer yok">ölü</span>`;
  return `<span class="alive"><span class="dot ok"></span>${p}</span>`;
}

// magnet kopyalama (delegasyon)
document.addEventListener("click", (e) => {
  const c = e.target.closest(".copy");
  if (c && c.dataset.magnet) copyMagnet(c.dataset.magnet);
});

// --- Tabs ---
document.querySelectorAll(".tab").forEach((t) => {
  t.addEventListener("click", () => {
    document.querySelectorAll(".tab").forEach((x) => x.classList.remove("active"));
    document.querySelectorAll(".view").forEach((x) => x.classList.add("hidden"));
    t.classList.add("active");
    $("view-" + t.dataset.view).classList.remove("hidden");
    if (t.dataset.view === "dashboard") { loadDashboard(); }
    if (t.dataset.view === "browse") { if (!browse.loaded) resetAndLoad(); }
    if (t.dataset.view === "analysis") loadAnalysis();
    if (t.dataset.view === "settings") loadSettings();
  });
});

// --- Stats polling ---
let scanning = false;
async function pollStats() {
  try {
    const s = await invoke("get_stats");
    scanning = s.scanning;
    $("t-fetched").textContent = nf(s.fetched);
    $("t-total").textContent = nf(s.total);
    pollStats._ema = pollStats._ema == null ? s.sample_rate : 0.7 * pollStats._ema + 0.3 * s.sample_rate;
    $("t-rate").textContent = nf(Math.round(pollStats._ema));
    $("t-unique").textContent = nf(s.unique);
    $("status-text").textContent = scanning ? "Tarıyor" : "Durdu";
    $("status-pill").className = "pill " + (scanning ? "on" : "off");
    $("btn-toggle").textContent = scanning ? "Taramayı Durdur" : "Taramayı Başlat";
    renderSemantic(s.semantic);
  } catch (e) {}
}

// --- Semantik arama durumu (ayarlar kartı + arama rozeti) ---
let semReady = false;
function renderSemantic(st) {
  if (!st) return;
  const el = $("sem-status"), prog = $("sem-progress"), bar = $("sem-bar");
  semReady = st.phase === "ready";
  let txt = "Kapalı"; let pct = 0; let showBar = false;
  if (st.phase === "downloading") {
    showBar = true;
    pct = st.total > 0 ? Math.round(100 * st.done / st.total) : 0;
    txt = `Model indiriliyor… ${st.file || ""} ${st.total > 0 ? pct + "%" : humanSize(st.done)}`;
  } else if (st.phase === "loading") { txt = "Model yükleniyor…"; }
  else if (st.phase === "ready") {
    txt = `Hazır — ${st.model} · ${st.device === "directml" ? "GPU (DirectML)" : "CPU"} · ${nf(st.indexed)} kayıt indekslendi (${st.index_mb} MB RAM)`;
  } else if (st.phase === "error") { txt = "Hata: " + (st.error || ""); }
  el.textContent = txt;
  el.className = "muted small" + (st.phase === "error" ? " err" : "");
  prog.classList.toggle("hidden", !showBar);
  bar.style.width = pct + "%";
  // Arama rozeti: semantik hazırsa göster (son aramanın modu ile).
  const badge = $("sem-badge");
  badge.classList.toggle("hidden", !semReady && st.phase !== "downloading" && st.phase !== "loading");
  if (st.phase !== "ready") { badge.className = "pill off"; $("sem-badge-text").textContent = st.phase === "downloading" ? "Semantik: indiriliyor" : st.phase === "loading" ? "Semantik: yükleniyor" : "Semantik"; }
  else if (!browse.lastMode) { badge.className = "pill on"; $("sem-badge-text").textContent = "Semantik hazır"; }
}

// --- Dashboard (chart + network) ---
const chart = { bucket: "hour", points: 48, series: [] };
async function loadDashboard() {
  try {
    const d = await invoke("dashboard", { bucket: chart.bucket, points: chart.points });
    chart.series = d.series || [];
    drawChart();
  } catch (e) {}
}
$("chart-seg").addEventListener("click", (e) => {
  const b = e.target.closest(".seg-btn"); if (!b) return;
  document.querySelectorAll("#chart-seg .seg-btn").forEach((x) => x.classList.remove("active"));
  b.classList.add("active");
  chart.bucket = b.dataset.bucket; chart.points = Number(b.dataset.points);
  loadDashboard();
});

function drawChart() {
  const el = $("chart");
  const W = el.clientWidth || 900, H = 190;
  const data = chart.series.slice().reverse(); // eski → yeni
  if (!data.length) {
    el.innerHTML = `<svg viewBox="0 0 ${W} ${H}"><text class="empty" x="${W / 2}" y="${H / 2}" text-anchor="middle">Henüz veri yok — tarama sürdükçe dolar</text></svg>`;
    return;
  }
  const padL = 10, padR = 10, padT = 14, padB = 24;
  const max = Math.max(...data.map((x) => x.count), 1);
  const n = data.length;
  const iw = W - padL - padR, ih = H - padT - padB;
  const xAt = (i) => padL + (n === 1 ? iw / 2 : (i * iw) / (n - 1));
  const yAt = (v) => padT + ih - (v / max) * ih;

  const pts = data.map((x, i) => [xAt(i), yAt(x.count)]);
  const line = pts.map((p, i) => (i ? "L" : "M") + p[0].toFixed(1) + " " + p[1].toFixed(1)).join(" ");
  const area = `M${pts[0][0].toFixed(1)} ${(padT + ih).toFixed(1)} ` +
    pts.map((p) => "L" + p[0].toFixed(1) + " " + p[1].toFixed(1)).join(" ") +
    ` L${pts[n - 1][0].toFixed(1)} ${(padT + ih).toFixed(1)} Z`;

  // yatay ızgara + y etiketleri (0, max/2, max)
  const grid = [0, 0.5, 1].map((f) => {
    const y = padT + ih - f * ih;
    return `<line class="grid-line" x1="${padL}" y1="${y.toFixed(1)}" x2="${W - padR}" y2="${y.toFixed(1)}" />
            <text class="axlabel" x="${padL}" y="${(y - 3).toFixed(1)}">${Math.round(f * max)}</text>`;
  }).join("");

  const fmt = (t) => chart.bucket === "day"
    ? new Date(t * 1000).toLocaleDateString("tr", { day: "2-digit", month: "2-digit" })
    : new Date(t * 1000).toLocaleString("tr", { day: "2-digit", month: "2-digit", hour: "2-digit" });
  const dots = pts.map((p, i) =>
    `<circle class="pt" cx="${p[0].toFixed(1)}" cy="${p[1].toFixed(1)}" r="2.6"><title>${fmt(data[i].t)} — ${nf(data[i].count)} keşif</title></circle>`
  ).join("");
  const xLabels =
    `<text class="axlabel" x="${padL}" y="${H - 6}" text-anchor="start">${fmt(data[0].t)}</text>` +
    `<text class="axlabel" x="${W - padR}" y="${H - 6}" text-anchor="end">${fmt(data[n - 1].t)}</text>`;

  el.innerHTML = `<svg viewBox="0 0 ${W} ${H}">
    <defs><linearGradient id="areaGrad" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0%" stop-color="#4f8cf7" stop-opacity="0.45" />
      <stop offset="100%" stop-color="#4f8cf7" stop-opacity="0" />
    </linearGradient></defs>
    ${grid}
    <path class="area" d="${area}" />
    <path class="line" d="${line}" />
    ${dots}${xLabels}
  </svg>`;
}
window.addEventListener("resize", () => { if (!$("view-dashboard").classList.contains("hidden")) drawChart(); });

// --- Analiz ---
async function loadAnalysis() {
  try {
    const d = await invoke("dashboard", { bucket: "hour", points: 1 });
    renderCats(d.overview);
  } catch (e) {}
}
function renderCats(ov) {
  if (!ov) return;
  const cats = ov.categories || [];
  const total = cats.reduce((a, c) => a + c.count, 0) || 1;
  const max = Math.max(...cats.map((c) => c.count), 1);
  $("cat-bars").innerHTML = cats.map((c) => {
    const cls = CAT[c.category] ? c.category : "other";
    const w = Math.round((c.count / max) * 100);
    return `<div class="catrow"><span>${catLabel(c.category)}</span>
      <div class="track"><div class="fill" style="width:${w}%;background:var(--c-${cls})"></div></div>
      <span class="r muted">${nf(c.count)}</span></div>`;
  }).join("") || `<span class="muted">Henüz veri yok</span>`;
  $("tbl-cats").innerHTML = cats.map((c) => {
    const pct = ((c.count / total) * 100).toFixed(1);
    return `<tr><td>${catChip(c.category)}</td><td class="r num">${nf(c.count)}</td><td class="r num">${humanSize(c.size)}</td><td class="r num">%${pct}</td></tr>`;
  }).join("");
  $("a-size").textContent = humanSize(ov.total_size);
  $("a-alive").textContent = nf(ov.alive);
  $("a-peers").textContent = nf(ov.total_peers);
  $("a-dead").textContent = nf(ov.dead);
}

// --- Network health ---
async function loadNetwork() {
  $("tbl-net").innerHTML = `<tr><td class="muted">Ölçülüyor…</td></tr>`;
  try {
    const r = await invoke("network_health");
    $("tbl-net").innerHTML = r.probes.map((p) => `
      <tr><td><span class="dot ${p.ok ? "ok" : ""}"></span>${esc(p.name)}</td>
        <td class="r num">${p.ok ? p.ms + " ms" : "erişilemedi"}</td></tr>`).join("");
  } catch (e) { $("tbl-net").innerHTML = `<tr><td class="muted">Hata</td></tr>`; }
}
$("btn-net").addEventListener("click", loadNetwork);

// --- Gözat & Ara (tek sıralanabilir, sayfalı tablo) ---
const browse = { q: "", cat: "all", sort: "", desc: true, offset: 0, PAGE: 60, hasMore: true, loading: false, loaded: false, mode: "auto", lastMode: "" };

function rowHtml(r, n) {
  return `<tr>
    <td class="r idx">${n}</td>
    <td class="name" title="${escAttr(r.name)}">${esc(r.name)}</td>
    <td>${catChip(r.category)}</td>
    <td class="r num">${humanSize(r.size)}</td>
    <td class="r num">${peerCell(r.peers)}</td>
    <td class="r num">${nf(r.files)}</td>
    <td class="r num" title="${escAttr(dateFull(r.last_seen))}">${dateShort(r.last_seen)}</td>
    <td class="r num"><span class="copy" data-magnet="${escAttr(r.magnet)}">magnet</span></td>
  </tr>`;
}

function resetAndLoad() {
  browse.offset = 0; browse.hasMore = true; browse.loaded = true;
  $("tbl-results").innerHTML = "";
  $("results-empty").classList.add("hidden");
  loadMore();
}

async function loadMore() {
  if (browse.loading || !browse.hasMore) return;
  browse.loading = true;
  $("results-loading").classList.remove("hidden");
  try {
    const r = await invoke("search", {
      query: browse.q, limit: browse.PAGE, offset: browse.offset,
      sort: browse.sort, desc: browse.desc,
      category: browse.cat, hideAdult: $("sf-adult").checked, onlyAlive: $("sf-alive").checked,
      mode: browse.mode,
    });
    const rows = r.results || [];
    if (browse.q && r.mode) {
      browse.lastMode = r.mode;
      const badge = $("sem-badge");
      badge.classList.toggle("hidden", !semReady);
      badge.className = "pill " + (r.mode === "fts" ? "off" : "on") + (semReady ? "" : " hidden");
      $("sem-badge-text").textContent = r.mode === "hybrid" ? "Hibrit (FTS + semantik)" : r.mode === "semantic" ? "Semantik" : "Yalnız FTS";
    } else if (!browse.q) { browse.lastMode = ""; }
    const start = browse.offset;
    if (rows.length) {
      $("tbl-results").insertAdjacentHTML("beforeend", rows.map((x, i) => rowHtml(x, start + i + 1)).join(""));
    }
    browse.offset += rows.length;
    browse.hasMore = rows.length === browse.PAGE;
    $("results-empty").classList.toggle("hidden", browse.offset > 0);
    $("result-count").textContent = browse.offset > 0
      ? `${nf(browse.offset)} sonuç${browse.hasMore ? "+" : ""}${browse.q ? "" : " (gözat)"}`
      : "";
  } catch (e) { toast("Yükleme hatası"); }
  finally { browse.loading = false; $("results-loading").classList.add("hidden"); }
  // İlk sayfa görünümü doldurmadıysa (scroll oluşmadıysa) bir sonrakini çek.
  const el = $("results-wrap");
  if (browse.hasMore && !browse.loading && el.scrollHeight <= el.clientHeight + 4) loadMore();
}

// sonsuz scroll
$("results-wrap").addEventListener("scroll", (e) => {
  const el = e.target;
  if (el.scrollTop + el.clientHeight >= el.scrollHeight - 140) loadMore();
});

// arama / temizle
$("search-form").addEventListener("submit", (e) => { e.preventDefault(); browse.q = $("q").value.trim(); resetAndLoad(); });
$("btn-clear").addEventListener("click", () => { $("q").value = ""; browse.q = ""; resetAndLoad(); });
$("sf-adult").addEventListener("change", resetAndLoad);
$("sf-alive").addEventListener("change", resetAndLoad);

// kategori sekmeleri
$("cattabs").addEventListener("click", (e) => {
  const b = e.target.closest(".cattab"); if (!b) return;
  document.querySelectorAll("#cattabs .cattab").forEach((x) => x.classList.remove("active"));
  b.classList.add("active");
  browse.cat = b.dataset.cat; resetAndLoad();
});

// sıralanabilir başlıklar
function updateSortUI() {
  document.querySelectorAll(".sortable-th").forEach((th) => {
    th.classList.remove("asc", "desc");
    let a = th.querySelector(".arrow"); if (!a) { a = document.createElement("span"); a.className = "arrow"; th.appendChild(a); }
    if (th.dataset.sort === browse.sort) { th.classList.add(browse.desc ? "desc" : "asc"); a.textContent = browse.desc ? "▼" : "▲"; }
    else a.textContent = "";
  });
}
document.querySelectorAll(".sortable-th").forEach((th) => {
  th.addEventListener("click", () => {
    const key = th.dataset.sort;
    if (browse.sort === key) browse.desc = !browse.desc;
    // ad/kategori varsayılan A→Z (artan); diğerleri büyük→küçük.
    else { browse.sort = key; browse.desc = key !== "name" && key !== "cat"; }
    updateSortUI(); resetAndLoad();
  });
});

// --- Start/Stop ---
$("btn-toggle").addEventListener("click", async () => {
  const wasScanning = scanning;
  try { await invoke(wasScanning ? "stop_scan" : "start_scan"); await pollStats(); toast(wasScanning ? "Tarama durdu" : "Tarama başladı"); }
  catch (e) { toast("İşlem başarısız: " + e); }
});

// --- Update ---
$("btn-update").addEventListener("click", async () => {
  toast("Güncelleme kontrol ediliyor…");
  try {
    const u = await invoke("check_update");
    if (u.available) {
      if (confirm(`Yeni sürüm: v${u.version}\n\n${u.notes || ""}\n\nŞimdi güncellensin mi?`)) {
        toast("İndiriliyor ve doğrulanıyor…"); await invoke("install_update");
      }
    } else { toast(u.error ? "Kontrol edilemedi: " + u.error : "En güncel sürümdesin."); }
  } catch (e) { toast("Güncelleme hatası: " + e); }
});

// --- Settings + engel kelimeleri ---
let blockKw = [];
function renderBlockChips() {
  const el = $("block-chips");
  if (!blockKw.length) { el.innerHTML = `<span class="block-empty">Henüz engel kelimesi yok.</span>`; return; }
  el.innerHTML = blockKw.map((k, i) =>
    `<span class="block-chip">${esc(k)}<span class="x" data-i="${i}">✕</span></span>`).join("");
}
function addBlockKw(raw) {
  const k = String(raw || "").trim().toLowerCase();
  if (!k) return;
  if (!blockKw.includes(k)) { blockKw.push(k); renderBlockChips(); }
}
$("block-form").addEventListener("submit", (e) => { e.preventDefault(); addBlockKw($("block-input").value); $("block-input").value = ""; });
$("block-chips").addEventListener("click", (e) => {
  const x = e.target.closest(".x"); if (!x) return;
  blockKw.splice(Number(x.dataset.i), 1); renderBlockChips();
});
document.querySelectorAll(".preset-chip").forEach((p) => p.addEventListener("click", () => addBlockKw(p.dataset.kw)));

async function loadSettings() {
  try {
    const s = await invoke("get_settings");
    $("set-rate").value = s.harvester_max_queries_per_sec;
    $("set-workers").value = s.fetch_workers;
    $("set-peers").value = s.fetch_peer_concurrency;
    $("set-port").value = s.harvester_port;
    $("set-autostart").checked = s.autostart;
    $("set-autoscan").checked = s.auto_scan;
    $("set-sem").checked = !!s.semantic_enabled;
    $("set-sem-tier").value = s.semantic_tier || "quality";
    $("set-sem-device").value = s.semantic_device || "auto";
    blockKw = Array.isArray(s.block_keywords) ? s.block_keywords.slice() : [];
    renderBlockChips();
    loadSettings._cur = s;
  } catch (e) {}
}
$("btn-save").addEventListener("click", async () => {
  const s = Object.assign({}, loadSettings._cur || {});
  s.harvester_max_queries_per_sec = Number($("set-rate").value) || 40;
  s.fetch_workers = Number($("set-workers").value) || 3;
  s.fetch_peer_concurrency = Number($("set-peers").value) || 6;
  s.harvester_port = Number($("set-port").value) || 0;
  s.autostart = $("set-autostart").checked;
  s.auto_scan = $("set-autoscan").checked;
  s.block_keywords = blockKw.slice();
  s.semantic_enabled = $("set-sem").checked;
  s.semantic_tier = $("set-sem-tier").value;
  s.semantic_device = $("set-sem-device").value;
  try {
    await invoke("set_settings", { settings: s });
    $("save-msg").textContent = "Kaydedildi ✓"; setTimeout(() => ($("save-msg").textContent = ""), 2000);
    browse.loaded = false; // filtre değişmiş olabilir → gözat yeniden yüklensin
    await pollStats();
  } catch (e) { $("save-msg").textContent = "Hata: " + e; }
});

// --- Init ---
(async function init() {
  try { const a = await invoke("app_info"); $("version").textContent = "v" + a.version; } catch (e) {}
  updateSortUI();
  await pollStats();
  await loadDashboard();
  setInterval(pollStats, 2500);
  setInterval(() => { if (!$("view-dashboard").classList.contains("hidden")) loadDashboard(); }, 20000);
})();
