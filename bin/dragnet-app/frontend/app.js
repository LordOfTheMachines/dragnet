// SPDX-License-Identifier: AGPL-3.0-only
"use strict";

const invoke = (cmd, args) => {
  const t = window.__TAURI__;
  if (!t || !t.core) return Promise.reject(new Error("Arayüz hazır değil"));
  return t.core.invoke(cmd, args || {});
};

const $ = (id) => document.getElementById(id);

function humanSize(bytes) {
  const u = ["B", "KB", "MB", "GB", "TB"];
  let n = Number(bytes) || 0, i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(n < 10 && i > 0 ? 1 : 0)} ${u[i]}`;
}

function toast(msg) {
  const el = $("toast");
  el.textContent = msg;
  el.classList.remove("hidden");
  clearTimeout(toast._t);
  toast._t = setTimeout(() => el.classList.add("hidden"), 2600);
}

function copyMagnet(magnet) {
  navigator.clipboard.writeText(magnet).then(
    () => toast("Magnet linki kopyalandı"),
    () => toast("Kopyalanamadı")
  );
}

function rowsHtml(items) {
  if (!items.length) return `<tr><td class="muted">Henüz veri yok</td></tr>`;
  return items.map((r) => `
    <tr>
      <td class="name" title="${escapeHtml(r.name)}">${escapeHtml(r.name)}</td>
      <td class="num">${humanSize(r.size)}</td>
      <td class="num"><span class="copy" data-magnet="${escapeAttr(r.magnet)}">magnet</span></td>
    </tr>`).join("");
}

function escapeHtml(s) { return String(s).replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c])); }
function escapeAttr(s) { return escapeHtml(s).replace(/"/g, "&quot;"); }

// --- Tabs ---
document.querySelectorAll(".tab").forEach((t) => {
  t.addEventListener("click", () => {
    document.querySelectorAll(".tab").forEach((x) => x.classList.remove("active"));
    document.querySelectorAll(".view").forEach((x) => x.classList.add("hidden"));
    t.classList.add("active");
    $("view-" + t.dataset.view).classList.remove("hidden");
    if (t.dataset.view === "dashboard") loadDashboard();
    if (t.dataset.view === "settings") loadSettings();
  });
});

// magnet kopyalama (delegasyon)
document.addEventListener("click", (e) => {
  const c = e.target.closest(".copy");
  if (c && c.dataset.magnet) copyMagnet(c.dataset.magnet);
});

// --- Stats polling ---
let scanning = false;
async function pollStats() {
  try {
    const s = await invoke("get_stats");
    scanning = s.scanning;
    $("t-fetched").textContent = s.fetched.toLocaleString("tr");
    $("t-total").textContent = s.total.toLocaleString("tr");
    // BEP-51 örnekleri patlamalı gelir; anlık ölçüm 0/tavan zıplar → EMA ile yumuşat.
    pollStats._ema = pollStats._ema == null ? s.sample_rate : 0.7 * pollStats._ema + 0.3 * s.sample_rate;
    $("t-rate").textContent = Math.round(pollStats._ema).toLocaleString("tr");
    $("t-unique").textContent = s.unique.toLocaleString("tr");
    const pill = $("status-pill");
    pill.textContent = scanning ? "Tarıyor" : "Durdu";
    pill.className = "pill " + (scanning ? "on" : "off");
    $("btn-toggle").textContent = scanning ? "Taramayı Durdur" : "Taramayı Başlat";
  } catch (e) { /* arayüz henüz hazır değil */ }
}

// --- Dashboard ---
async function loadDashboard() {
  try {
    const d = await invoke("dashboard");
    $("tbl-seen").innerHTML = rowsHtml(d.top_seen);
    $("tbl-size").innerHTML = rowsHtml(d.top_size);
    $("tbl-recent").innerHTML = rowsHtml(d.recent);
    drawChart(d.daily);
  } catch (e) {}
}

function drawChart(daily) {
  const el = $("chart");
  const data = (daily || []).slice().reverse(); // eski → yeni
  if (!data.length) { el.innerHTML = `<span class="muted">Veri yok</span>`; return; }
  const max = Math.max(...data.map((x) => x.count), 1);
  el.innerHTML = data.map((x) => {
    const h = Math.round((x.count / max) * 100);
    const day = new Date(x.day * 1000).toLocaleDateString("tr");
    return `<div class="bar" style="height:${h}%" title="${day}: ${x.count}"></div>`;
  }).join("");
}

// --- Network health ---
async function loadNetwork() {
  $("tbl-net").innerHTML = `<tr><td class="muted">Ölçülüyor…</td></tr>`;
  try {
    const r = await invoke("network_health");
    $("tbl-net").innerHTML = r.probes.map((p) => `
      <tr>
        <td><span class="dot ${p.ok ? "ok" : "bad"}"></span>${escapeHtml(p.name)}</td>
        <td class="num">${p.ok ? p.ms + " ms" : "erişilemedi"}</td>
      </tr>`).join("");
  } catch (e) { $("tbl-net").innerHTML = `<tr><td class="muted">Hata</td></tr>`; }
}
$("btn-net").addEventListener("click", loadNetwork);

// --- Search ---
$("search-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const q = $("q").value.trim();
  if (!q) return;
  try {
    const r = await invoke("search", { query: q, limit: 100 });
    const rows = r.results;
    $("search-empty").classList.toggle("hidden", rows.length > 0);
    $("tbl-results").innerHTML = rows.map((x) => `
      <tr>
        <td class="name" title="${escapeHtml(x.name)}">${escapeHtml(x.name)}</td>
        <td class="num">${humanSize(x.size)}</td>
        <td class="num">${x.files}</td>
        <td class="num">${x.seen}</td>
        <td class="num"><span class="copy" data-magnet="${escapeAttr(x.magnet)}">magnet</span></td>
      </tr>`).join("");
  } catch (e) { toast("Arama hatası"); }
});

// --- Start/Stop ---
$("btn-toggle").addEventListener("click", async () => {
  try {
    await invoke(scanning ? "stop_scan" : "start_scan");
    await pollStats();
    toast(scanning ? "Tarama başladı" : "Tarama durdu");
  } catch (e) { toast("İşlem başarısız: " + e); }
});

// --- Update ---
$("btn-update").addEventListener("click", async () => {
  toast("Güncelleme kontrol ediliyor…");
  try {
    const u = await invoke("check_update");
    if (u.available) {
      if (confirm(`Yeni sürüm var: v${u.version}\n\n${u.notes || ""}\n\nŞimdi güncellensin mi?`)) {
        toast("İndiriliyor ve doğrulanıyor…");
        await invoke("install_update");
      }
    } else {
      toast(u.error ? "Kontrol edilemedi: " + u.error : "En güncel sürümdesin.");
    }
  } catch (e) { toast("Güncelleme hatası: " + e); }
});

// --- Settings ---
async function loadSettings() {
  try {
    const s = await invoke("get_settings");
    $("s-rate").value = s.harvester_max_queries_per_sec;
    $("s-workers").value = s.fetch_workers;
    $("s-peers").value = s.fetch_peer_concurrency;
    $("s-port").value = s.harvester_port;
    $("s-autostart").checked = s.autostart;
    $("s-autoscan").checked = s.auto_scan;
    loadSettings._cur = s;
  } catch (e) {}
}
$("btn-save").addEventListener("click", async () => {
  const s = Object.assign({}, loadSettings._cur || {});
  s.harvester_max_queries_per_sec = Number($("s-rate").value) || 40;
  s.fetch_workers = Number($("s-workers").value) || 2;
  s.fetch_peer_concurrency = Number($("s-peers").value) || 6;
  s.harvester_port = Number($("s-port").value) || 0;
  s.autostart = $("s-autostart").checked;
  s.auto_scan = $("s-autoscan").checked;
  try {
    await invoke("set_settings", { settings: s });
    $("save-msg").textContent = "Kaydedildi ✓";
    setTimeout(() => ($("save-msg").textContent = ""), 2000);
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
