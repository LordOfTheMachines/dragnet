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

function toast(msg) {
  const el = $("toast"); el.textContent = msg; el.classList.remove("hidden");
  clearTimeout(toast._t); toast._t = setTimeout(() => el.classList.add("hidden"), 2600);
}
function copyMagnet(m) { navigator.clipboard.writeText(m).then(() => toast("Magnet kopyalandı"), () => toast("Kopyalanamadı")); }

function peerCell(p) {
  if (p == null) return `<span class="muted" title="Henüz kontrol edilmedi">–</span>`;
  if (p <= 0) return `<span class="dead" title="Canlı peer yok">ölü</span>`;
  return `<span class="alive"><span class="dot ok"></span>${p}</span>`;
}

function rowsHtml(items) {
  if (!items.length) return `<tr><td colspan="5" class="muted">Henüz veri yok — tarama sürdükçe dolar</td></tr>`;
  return items.map((r) => `
    <tr>
      <td class="name" title="${escAttr(r.name)}">${esc(r.name)}</td>
      <td>${catChip(r.category)}</td>
      <td class="r num">${humanSize(r.size)}</td>
      <td class="r num">${peerCell(r.peers)}</td>
      <td class="r num"><span class="copy" data-magnet="${escAttr(r.magnet)}">magnet</span></td>
    </tr>`).join("");
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
    if (t.dataset.view === "dashboard") loadDashboard();
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
  } catch (e) {}
}

// --- Dashboard ---
let lastOverview = null;
async function loadDashboard() {
  try {
    const d = await invoke("dashboard", { hideAdult: $("f-adult").checked, onlyAlive: $("f-alive").checked });
    $("tbl-seen").innerHTML = rowsHtml(d.top_seen);
    $("tbl-size").innerHTML = rowsHtml(d.top_size);
    $("tbl-recent").innerHTML = rowsHtml(d.recent);
    lastOverview = d.overview;
    drawChart(d.hourly);
  } catch (e) {}
}
$("f-adult").addEventListener("change", loadDashboard);
$("f-alive").addEventListener("change", loadDashboard);

function drawChart(hourly) {
  const el = $("chart");
  const data = (hourly || []).slice().reverse(); // eski → yeni
  if (!data.length) { el.innerHTML = `<span class="muted">Veri yok</span>`; $("chart-start").textContent = ""; return; }
  const max = Math.max(...data.map((x) => x.count), 1);
  el.innerHTML = data.map((x) => {
    const h = Math.max(2, Math.round((x.count / max) * 100));
    const d = new Date(x.hour * 1000);
    const t = d.toLocaleString("tr", { day: "2-digit", month: "2-digit", hour: "2-digit" });
    return `<div class="bar" style="height:${h}%" title="${t} — ${x.count} keşif"></div>`;
  }).join("");
  $("chart-start").textContent = new Date(data[0].hour * 1000).toLocaleString("tr", { day: "2-digit", month: "2-digit", hour: "2-digit" });
}

// --- Analiz ---
async function loadAnalysis() {
  try {
    const d = await invoke("dashboard", { hideAdult: false, onlyAlive: false });
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

// --- Search ---
$("search-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const q = $("q").value.trim();
  if (!q) return;
  try {
    const r = await invoke("search", {
      query: q, limit: 200,
      hideAdult: $("sf-adult").checked, onlyAlive: $("sf-alive").checked,
      category: $("s-cat").value,
    });
    const rows = r.results;
    $("search-empty").classList.toggle("hidden", rows.length > 0);
    $("tbl-results").innerHTML = rows.map((x) => `
      <tr>
        <td class="name" title="${escAttr(x.name)}">${esc(x.name)}</td>
        <td>${catChip(x.category)}</td>
        <td class="r num">${humanSize(x.size)}</td>
        <td class="r num">${peerCell(x.peers)}</td>
        <td class="r num">${x.files}</td>
        <td class="r num"><span class="copy" data-magnet="${escAttr(x.magnet)}">magnet</span></td>
      </tr>`).join("");
  } catch (e) { toast("Arama hatası"); }
});

// --- Start/Stop ---
$("btn-toggle").addEventListener("click", async () => {
  try { await invoke(scanning ? "stop_scan" : "start_scan"); await pollStats(); toast(scanning ? "Tarama başladı" : "Tarama durdu"); }
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

// --- Settings ---
async function loadSettings() {
  try {
    const s = await invoke("get_settings");
    $("set-rate").value = s.harvester_max_queries_per_sec;
    $("set-workers").value = s.fetch_workers;
    $("set-peers").value = s.fetch_peer_concurrency;
    $("set-port").value = s.harvester_port;
    $("set-autostart").checked = s.autostart;
    $("set-autoscan").checked = s.auto_scan;
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
  try {
    await invoke("set_settings", { settings: s });
    $("save-msg").textContent = "Kaydedildi ✓"; setTimeout(() => ($("save-msg").textContent = ""), 2000);
    await pollStats();
  } catch (e) { $("save-msg").textContent = "Hata: " + e; }
});

// --- Init ---
(async function init() {
  try { const a = await invoke("app_info"); $("version").textContent = "v" + a.version; } catch (e) {}
  await pollStats();
  await loadDashboard();
  setInterval(pollStats, 2500);
  setInterval(() => { if (!$("view-dashboard").classList.contains("hidden")) loadDashboard(); }, 15000);
})();
