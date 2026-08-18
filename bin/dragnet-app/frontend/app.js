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
    renderFetch._hints = s.peer_hints;
    renderFetch(s.fetch, s.queue, scanning);
    renderReach(s, scanning);
  } catch (e) {}
}

// --- DHT erişilebilirliği (port yönlendirme kontrolü) ---
const reach = { prev: null, prevT: 0, ema: null, since: 0 };
function renderReach(s, on) {
  const set = (id, v) => { $(id).textContent = v; };
  const pill = $("reach-pill"), txt = $("reach-text");
  if (!on) {
    ["r-port","r-inrate","r-getpeers","r-announce","r-hints","r-public"].forEach((id) => set(id, "–"));
    pill.className = "pill off"; txt.textContent = "Tarama durdu"; $("reach-detail").textContent = "";
    reach.prev = null; reach.ema = null; reach.since = 0;
    return;
  }
  const now = Date.now();
  const q = Number(s.queries_seen || 0);
  if (reach.prev != null && now > reach.prevT) {
    const perMin = (q - reach.prev) / ((now - reach.prevT) / 60000);
    reach.ema = reach.ema == null ? perMin : 0.6 * reach.ema + 0.4 * perMin;
  } else if (reach.prev == null) { reach.since = now; }
  reach.prev = q; reach.prevT = now;
  const port = (s.harvester_addr || "").split(":").pop() || "–";
  set("r-port", port);
  set("r-inrate", reach.ema == null ? "…" : nf(Math.round(reach.ema)));
  set("r-getpeers", nf(s.get_peers_seen || 0));
  set("r-announce", nf(s.announce_seen || 0));
  set("r-hints", nf(s.peer_hints || 0));
  const dc = s.dht_client || {};
  set("r-public", dc.public ? String(dc.public).split(":")[0] : "–");
  const uptimeMin = (now - reach.since) / 60000;
  const rate = reach.ema == null ? 0 : reach.ema;
  let cls = "off", label = "Ölçülüyor…", detail = "";
  if (rate >= 20 || (s.announce_seen || 0) > 5) {
    cls = "on"; label = "Erişilebilir — pasif hasat aktif";
    detail = `Dışarıdan gelen DHT sorguları alınıyor (${nf(Math.round(rate))}/dk). Port yönlendirmesi çalışıyor; announce/get_peers sinyalleri sıcak kuyruğu besler.`;
  } else if (uptimeMin >= 3) {
    cls = "off"; label = "NAT arkasında görünüyor";
    detail = `${uptimeMin.toFixed(0)} dakikadır neredeyse hiç gelen sorgu yok (${nf(Math.round(rate))}/dk). Modemde harvester UDP portunu (${port}) bu bilgisayara yönlendirin ve Ayarlar'da aynı portu seçin; qBittorrent aynı portu kullanıyorsa birini değiştirin. Aktif hasat (BEP-51) ve peer ipuçları yine çalışır, ama popüler torrent'lerin en güçlü sinyali (announce) gelmez.`;
  } else {
    detail = "İlk 3 dakika bekleniyor: port dışarıdan açıksa gelen sorgu sayısı hızla artmalı.";
  }
  if (dc.firewalled === false && dc.public) detail += ` mainline istemcisi (port ${dc.port}) dış adresi ${dc.public} olarak görüyor ve NAT arkasında değil.`;
  pill.className = "pill " + cls; txt.textContent = label; $("reach-detail").textContent = detail;
}

// --- Metadata çekim kartı (Faz E) ---
function renderFetch(f, q, on) {
  const set = (id, v) => { $(id).textContent = v; };
  if (!on || !f) {
    ["f-rate","f-success","f-attempts","f-avg","f-hot","f-pending"].forEach((id) => set(id, "–"));
    $("fetch-summary").textContent = on ? "" : "Tarama durdu";
    $("fetch-detail").textContent = "";
    return;
  }
  const rate = f.attempts ? Math.round(100 * f.ok / f.attempts) : 0;
  set("f-rate", q ? nf(q.fetched_last_hour) : "–");
  set("f-success", f.attempts ? `%${rate}` : "–");
  set("f-attempts", nf(f.attempts));
  set("f-avg", f.attempts ? (f.avg_ms >= 1000 ? (f.avg_ms / 1000).toFixed(1) + " sn" : f.avg_ms + " ms") : "–");
  set("f-hot", q ? nf(q.hot) : "–");
  set("f-pending", q ? `${nf(q.pending)} / ${nf(q.unreachable)}` : "–");
  const perHour = f.attempts && f.avg_ms ? Math.round(3600000 / f.avg_ms) : 0;
  $("fetch-summary").textContent = f.attempts ? `${nf(f.ok)} başarılı · ${nf(f.no_peers)} peer yok · ${nf(f.all_peers_failed)} peer başarısız · ${nf(renderFetch._hints || 0)} peer ipucu` : "";
  $("fetch-detail").textContent = f.attempts
    ? `Ort. ${f.avg_peers.toFixed(1)} peer/çekim. Peer bulunamayan çekimler DHT'de o an kimsenin sunmadığı (ölü) torrent'lerdir; ` +
      `peer bulunup başarısız olanlar çoğunlukla NAT arkasındaki ya da metadata paylaşmayan peer'lerdir. Kuyruk sıcak › popüler › taze sırasıyla işlenir; başarısızlar 6 saat sonra tekrar denenir (en çok 3).`
    : "Çekim istatistikleri tarama sürdükçe dolar.";
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
    txt = `Hazır — ${st.model} · ${st.device === "directml" ? "GPU (DirectML)" : "CPU"} · ${nf(st.indexed)} kayıt indekslendi (${st.index_mb} MB RAM)` + (st.rerank ? ` · yeniden sıralayıcı aktif (${st.rerank})` : "");
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
  const W = el.clientWidth || 900, H = 210;
  const data = chart.series.slice().reverse(); // eski → yeni (dolu seri: boş kovalar 0)
  if (!data.length) {
    el.innerHTML = `<svg viewBox="0 0 ${W} ${H}"><text class="empty" x="${W / 2}" y="${H / 2}" text-anchor="middle">Henüz veri yok — tarama sürdükçe dolar</text></svg>`;
    return;
  }
  const isDay = chart.bucket === "day";
  const padL = 34, padR = 14, padT = 14, padB = 30;
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

  // Y ızgarası: 0 / ½ / max (tam sayı; max küçükse yalnız 0 ve max).
  const yTicks = max >= 2 ? [0, 0.5, 1] : [0, 1];
  const grid = yTicks.map((f) => {
    const y = padT + ih - f * ih;
    return `<line class="grid-line" x1="${padL}" y1="${y.toFixed(1)}" x2="${W - padR}" y2="${y.toFixed(1)}" />
            <text class="axlabel" x="${padL - 6}" y="${(y + 3.5).toFixed(1)}" text-anchor="end">${Math.round(f * max)}</text>`;
  }).join("");

  // X ekseni: anlamlı zaman işaretleri — saatlik: her 6 saat (tam saatte), günlük: her N gün.
  const d0 = (t) => new Date(t * 1000);
  const two = (v) => String(v).padStart(2, "0");
  const fmtTick = (t) => { const d = d0(t); return isDay ? `${two(d.getDate())}.${two(d.getMonth() + 1)}` : (d.getHours() === 0 ? `${two(d.getDate())}.${two(d.getMonth() + 1)}` : `${two(d.getHours())}:00`); };
  const fmtFull = (t) => { const d = d0(t); return isDay
    ? d.toLocaleDateString("tr", { day: "2-digit", month: "2-digit", year: "numeric" })
    : `${d.toLocaleDateString("tr", { day: "2-digit", month: "2-digit" })} ${two(d.getHours())}:00–${two((d.getHours() + 1) % 24)}:00`; };
  const step = isDay ? Math.max(1, Math.round(n / 8)) : (n > 96 ? 24 : 6);
  const ticks = [];
  data.forEach((x, i) => {
    const d = d0(x.t);
    const on = isDay ? ((n - 1 - i) % step === 0) : (d.getHours() % step === 0);
    if (on) ticks.push(`<line class="tick" x1="${xAt(i).toFixed(1)}" y1="${padT + ih}" x2="${xAt(i).toFixed(1)}" y2="${padT + ih + 4}" />
      <text class="axlabel" x="${xAt(i).toFixed(1)}" y="${H - 8}" text-anchor="middle">${fmtTick(x.t)}</text>`);
  });
  // "şimdi" vurgusu (en sağ kova = içinde bulunulan saat/gün, henüz dolmamış).
  const nowX = xAt(n - 1).toFixed(1);
  const nowMark = `<line class="now" x1="${nowX}" y1="${padT}" x2="${nowX}" y2="${padT + ih}" /><text class="axlabel now-label" x="${nowX}" y="${padT - 3}" text-anchor="end">şimdi</text>`;

  const dots = pts.map((p, i) =>
    `<circle class="pt${data[i].count ? "" : " zero"}" cx="${p[0].toFixed(1)}" cy="${p[1].toFixed(1)}" r="${n > 100 ? 1.8 : 2.6}" data-i="${i}"></circle>`
  ).join("");
  // Hover için görünmez dikey şeritler (her kova) → ipucu.
  const bandW = n > 1 ? iw / (n - 1) : iw;
  const bands = data.map((x, i) =>
    `<rect class="band" data-i="${i}" x="${(xAt(i) - bandW / 2).toFixed(1)}" y="${padT}" width="${bandW.toFixed(1)}" height="${ih}" />`
  ).join("");

  el.innerHTML = `<svg viewBox="0 0 ${W} ${H}">
    <defs><linearGradient id="areaGrad" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0%" stop-color="#4f8cf7" stop-opacity="0.45" />
      <stop offset="100%" stop-color="#4f8cf7" stop-opacity="0" />
    </linearGradient></defs>
    ${grid}${ticks.join("")}
    <line class="axis" x1="${padL}" y1="${padT + ih}" x2="${W - padR}" y2="${padT + ih}" />
    <path class="area" d="${area}" />
    <path class="line" d="${line}" />
    ${nowMark}${dots}${bands}
    <g class="hover hidden"><line class="cursor" x1="0" y1="${padT}" x2="0" y2="${padT + ih}" /><circle class="focus" r="4.5" /></g>
  </svg><div class="tip hidden"></div>`;

  const svg = el.querySelector("svg"), tip = el.querySelector(".tip"), hover = el.querySelector(".hover");
  const total = data.reduce((a, x) => a + x.count, 0);
  const show = (i, ev) => {
    const x = pts[i][0], y = pts[i][1];
    hover.classList.remove("hidden");
    hover.querySelector(".cursor").setAttribute("x1", x); hover.querySelector(".cursor").setAttribute("x2", x);
    const f = hover.querySelector(".focus"); f.setAttribute("cx", x); f.setAttribute("cy", y);
    tip.innerHTML = `<b>${nf(data[i].count)}</b> keşif<br><span class="muted">${fmtFull(data[i].t)}</span>`;
    tip.classList.remove("hidden");
    const r = el.getBoundingClientRect(); const px = ev.clientX - r.left;
    tip.style.left = Math.min(Math.max(px + 12, 0), r.width - 150) + "px";
  };
  svg.addEventListener("mousemove", (ev) => { const b = ev.target.closest(".band, .pt"); if (b) show(Number(b.dataset.i), ev); });
  svg.addEventListener("mouseleave", () => { hover.classList.add("hidden"); tip.classList.add("hidden"); });
  const cap = $("chart-caption"); if (cap) cap.textContent = isDay ? `Son ${n} gün · toplam ${nf(total)} keşif` : `Son ${n} saat · toplam ${nf(total)} keşif`;
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
      mode: browse.mode, hideGarbled: $("sf-garbled").checked,
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
    $("result-count").innerHTML = browse.offset > 0
      ? `${nf(browse.offset)} sonuç${browse.hasMore ? "+" : ""}${browse.q ? "" : " (gözat)"}` +
        // Sorgu varken alaka dışı sıralama seçiliyse hatırlat: alakalılar listenin
        // ortasına düşebilir (Faz E kullanıcı geri bildirimi).
        (browse.q && browse.sort ? ` · <a href="#" id="sort-reset" title="Sıralamayı kaldır, alaka sırasına dön">alaka sırasına dön</a>` : "")
      : "";
    const sr = $("sort-reset");
    if (sr) sr.addEventListener("click", (ev) => { ev.preventDefault(); browse.sort = ""; browse.desc = true; updateSortUI(); resetAndLoad(); });
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
$("sf-garbled").addEventListener("change", resetAndLoad);

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
    $("set-sem-rerank").checked = s.semantic_rerank !== false;
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
  s.semantic_rerank = $("set-sem-rerank").checked;
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
