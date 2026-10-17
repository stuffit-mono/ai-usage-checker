const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
let snap = { accounts: [], statuses: {}, settings: {}, autostart: false };
let view = "usage";

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

// ---------- 시간 표시 ----------

function fmtDuration(ms) {
  if (ms <= 0) return "곧";
  const s = Math.floor(ms / 1000);
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return `${d}일 ${h}시간`;
  if (h > 0) return `${h}시간 ${m}분`;
  if (m > 0) return `${m}분 ${s % 60}초`;
  return `${s}초`;
}

const timeFmt = new Intl.DateTimeFormat("ko-KR", { hour: "numeric", minute: "2-digit" });
const WEEKDAYS = "일월화수목금토";

function fmtClock(date) {
  const now = new Date();
  const sameDay = date.toDateString() === now.toDateString();
  if (sameDay) return timeFmt.format(date);
  return `${date.getMonth() + 1}/${date.getDate()}(${WEEKDAYS[date.getDay()]}) ${timeFmt.format(date)}`;
}

function fmtAgo(iso) {
  const diff = Date.now() - new Date(iso).getTime();
  if (diff < 60_000) return "방금 갱신";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}분 전 갱신`;
  return `${Math.floor(diff / 3_600_000)}시간 전 갱신`;
}

// ---------- 사용량 화면 ----------

/** 한도에 가까워질수록 파랑 → 초록 → 노랑 → 빨강. 노랑·빨강 기준은 설정의 알림 기준을 따른다. */
function level(p) {
  const s = snap.settings;
  const warn = s.warn_percent ?? 80;
  if (p >= (s.crit_percent ?? 95)) return "lvl-crit";
  if (p >= warn) return "lvl-warn";
  if (p >= Math.min(50, warn)) return "lvl-mid";
  return "lvl-low";
}

const HOUR = 3_600_000;

/** 한도 구간의 전체 길이(ms). 지난 시간 막대에 쓴다. */
function windowMs(l) {
  if (l.window_secs) return l.window_secs * 1000;
  if (l.key === "session") return 5 * HOUR;
  if (l.key === "weekly" || l.key.startsWith("model:")) return 7 * 24 * HOUR;
  return null;
}

const MAIN_KEYS = ["session", "weekly", "budget", "spend"];

const SERVICES = {
  claude_web: { name: "Claude", cls: "svc-claude", logo: "claude" },
  claude_code: { name: "Claude", cls: "svc-claude", logo: "claude" },
  chatgpt_web: { name: "ChatGPT", cls: "svc-chatgpt", logo: "openai" },
  anthropic_admin: { name: "Claude API", cls: "svc-api", logo: "claude" },
  openai_admin: { name: "OpenAI API", cls: "svc-api", logo: "openai" },
};

/** 서비스 로고 SVG (ui/logos.js). 색은 글자색(currentColor)을 따른다. */
function logoSvg(svc) {
  const d = svc && LOGO_PATHS[svc.logo];
  if (!d) return "";
  const rule = svc.logo === "openai" ? "evenodd" : "nonzero";
  return `<svg class="logo" viewBox="0 0 24 24" aria-hidden="true"><path fill="currentColor" fill-rule="${rule}" d="${d}"/></svg>`;
}
const isApi = (a) => a.source.kind === "anthropic_admin" || a.source.kind === "openai_admin";

/** 초기화 시점을 나타내는 깃발 (깃대 왼쪽, 천 오른쪽) */
const FLAG_ICON = `<svg class="flag" viewBox="0 0 16 16" aria-hidden="true"><path class="pole" d="M3.5 1.5v13"/><path class="cloth" d="M4 2h8l-2 3 2 3H4z"/></svg>`;

const CLOCK_ICON = `<svg class="clock" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/></svg>`;

function limitHtml(l, main) {
  const p = Math.max(0, Math.min(100, l.percent));
  const win = l.resets_at ? windowMs(l) : null;
  let eta = "";
  if (main) {
    if (l.percent >= 100) eta = `<span class="eta danger">한도 도달</span>`;
    else if (l.eta) eta = `<span class="eta danger" data-eta="${esc(l.eta)}"></span>`;
    else if (l.safe_until_reset) eta = `<span class="eta safe">초기화 전 한도 도달 안 함</span>`;
  }

  // 주요 한도는 사용량 막대 바로 아래에 같은 폭의 "지난 시간" 막대를 둔다.
  // 두 막대를 위아래로 비교하면 시간보다 빨리 쓰고 있는지 바로 보인다.
  let time = "";
  if (main && win) {
    time = `
      <div class="tgroup" data-tbar="${esc(l.resets_at)}" data-win="${win}">
        <div class="journey" title="초기화까지의 여정: 점이 깃발에 닿으면 초기화">
          <div class="road"><i class="done"></i><b class="dot"></b></div>
          ${FLAG_ICON}
        </div>
        <div class="sub">
          <span class="tleft">${CLOCK_ICON}<b></b><span class="at" data-reset-at="${esc(l.resets_at)}"></span></span>
          ${eta}
        </div>
      </div>`;
  } else if (l.resets_at || eta) {
    const reset = l.resets_at ? `<span data-reset="${esc(l.resets_at)}"></span>` : "";
    time = `<div class="sub">${reset}${eta}</div>`;
  }

  const value = l.display ?? `${Math.round(l.percent)}%`;
  const bar = l.no_bar
    ? ""
    : `<div class="bar" title="사용량 ${Math.round(l.percent)}%"><i style="width:${p}%"></i></div>`;
  return `
    <div class="limit ${main ? "main" : ""} ${l.no_bar ? "" : level(l.percent)}">
      <div class="row"><span class="lbl">${esc(l.label)}</span><span class="pct">${esc(value)}</span></div>
      ${bar}
      ${time}
    </div>`;
}

/** 로그인이 만료됐을 때 "다시 로그인" 버튼이 할 동작 */
const RELOGIN = { claude_web: "login", chatgpt_web: "login-chatgpt" };

function cardHtml(a) {
  const st = snap.statuses[a.id];
  const limits = st?.limits ?? [];
  const primary = limits.filter((l) => MAIN_KEYS.includes(l.key));
  const others = limits.filter((l) => !MAIN_KEYS.includes(l.key));
  const svc = SERVICES[a.source.kind] ?? { name: "", cls: "" };
  const badges = [
    a.plan ? `<span class="badge">${esc(a.plan)}</span>` : "",
    st?.badge ? `<span class="badge muted">${esc(st.badge)}</span>` : "",
    st?.extra_usage ? `<span class="badge muted" title="한도를 넘으면 추가 요금으로 계속 사용">추가 사용</span>` : "",
    a.source.kind === "claude_code" ? `<span class="badge muted">Code</span>` : "",
  ].join("");

  let body;
  if (!st) {
    body = `<div class="sub">불러오는 중…</div>`;
  } else {
    body = primary.map((l) => limitHtml(l, true)).join("");
    if (others.length) body += `<div class="models">${others.map((l) => limitHtml(l, false)).join("")}</div>`;
    if (!limits.length && st.ok) body = `<div class="sub">표시할 한도 정보가 없습니다.</div>`;
  }

  let err = "";
  if (st && !st.ok) {
    const action = st.error_kind === "auth" ? RELOGIN[a.source.kind] : null;
    err = `<div class="err"><span>${esc(st.error)}</span>${action ? `<button data-action="${action}">다시 로그인</button>` : ""}</div>`;
  }

  return `
    <section class="card ${st && !st.ok ? "stale" : ""}">
      <div class="card-head">
        <span class="svc ${svc.cls}">${logoSvg(svc)}${esc(svc.name)}</span>
        <span class="name" title="${esc(a.label)}">${esc(a.label)}</span>${badges}
      </div>
      ${body}${trendHtml(st?.trend)}${err}
    </section>`;
}

/** 최근 7일 추이: 2시간 칸마다 최대 사용률을 막대로 그린다. */
function trendHtml(t) {
  if (!t || !t.buckets?.length) return "";
  const n = t.buckets.length;
  const H = 24;
  const start = new Date(t.start).getTime();
  const stepMs = t.bucket_hours * HOUR;
  const bars = t.buckets
    .map((v, i) => {
      if (v == null) return "";
      const h = Math.max(1, (Math.min(100, v) / 100) * H);
      const from = new Date(start + i * stepMs);
      const tip = `${fmtClock(from)}부터 ${t.bucket_hours}시간 · 최대 ${Math.round(v)}%`;
      return `<rect class="${level(v)}" x="${i + 0.1}" y="${H - h}" width="0.8" height="${h}"><title>${esc(tip)}</title></rect>`;
    })
    .join("");
  // 하루 경계(현지 자정)에 옅은 세로선
  const lines = [];
  for (let i = 1; i < n; i++) {
    const d = new Date(start + i * stepMs);
    if (d.getHours() < t.bucket_hours && new Date(start + (i - 1) * stepMs).getDate() !== d.getDate()) {
      lines.push(`<line x1="${i}" x2="${i}" y1="0" y2="${H}"/>`);
    }
  }
  const what = t.key === "session" ? "5시간 사용률" : "주간 사용률";
  return `
    <div class="trend">
      <div class="trend-head"><span>최근 7일 ${what}</span><span>2시간 단위 최대값</span></div>
      <svg viewBox="0 0 ${n} ${H}" preserveAspectRatio="none" aria-label="최근 7일 ${what} 추이">${lines.join("")}${bars}</svg>
      <div class="trend-axis"><span>7일 전</span><span>지금</span></div>
    </div>`;
}

/** 미니 모드 한 줄에 쓰는 짧은 이름 */
const SHORT = { session: "5h", weekly: "주", budget: "월", spend: "월" };

/** 미니 모드: 계정당 한 줄 (이름 + 주요 한도 2개) */
function compactRowHtml(a) {
  const st = snap.statuses[a.id];
  const svc = SERVICES[a.source.kind] ?? { name: "", cls: "" };
  const cells = (st?.limits ?? [])
    .filter((l) => MAIN_KEYS.includes(l.key))
    .slice(0, 2)
    .map((l) => {
      const value = l.display ?? `${Math.round(l.percent)}%`;
      const reset = l.resets_at ? ` · ${fmtClock(new Date(l.resets_at))} 초기화` : "";
      const bar = l.no_bar
        ? ""
        : `<div class="bar"><i style="width:${Math.max(0, Math.min(100, l.percent))}%"></i></div>`;
      return `
        <div class="cell ${l.no_bar ? "" : level(l.percent)}" title="${esc(l.label)} ${esc(value)}${esc(reset)}">
          <span class="k">${SHORT[l.key] ?? ""}</span>${bar}<span class="pct">${esc(value)}</span>
        </div>`;
    })
    .join("");
  const warn = st && !st.ok ? `<span class="warn" title="${esc(st.error)}">⚠</span>` : "";
  return `
    <section class="crow ${st && !st.ok ? "stale" : ""}">
      <span class="svc ${svc.cls}" title="${esc(svc.name)}">${logoSvg(svc)}</span>
      <span class="name" title="${esc(a.label)}">${esc(a.label)}</span>${warn}
      <div class="cells">${cells || `<span class="sub">불러오는 중…</span>`}</div>
    </section>`;
}

/** 지금 보이는 화면 내용 전체의 높이(px). 창이 내용보다 커도 정확히 잰다. */
function contentHeight() {
  const main = view === "usage" ? $("view-usage") : $("view-settings");
  const last = main.lastElementChild;
  // offsetTop은 .shell 기준이고 스크롤과 무관하다
  const bottom = last ? last.offsetTop + last.offsetHeight : main.offsetTop;
  const pad = parseFloat(getComputedStyle(main).paddingBottom) || 0;
  return Math.ceil(Math.min(bottom + pad + 2, window.screen.availHeight * 0.9));
}

/** 창이 내용보다 길어지지 않게 한다.
 *  - 최대 높이는 항상 내용 높이로 묶는다(더 늘릴 수 없음)
 *  - 미니 모드이거나, 직전에 내용에 딱 맞춰져 있었으면 내용이 바뀔 때 함께 늘고 준다
 *  - 사용자가 일부러 줄여 둔 창은 그대로 두고 스크롤로 본다 */
let lastNeed = 0;

function fitWindow() {
  requestAnimationFrame(() => {
    const need = contentHeight();
    if (need === lastNeed) return;
    const fitted = lastNeed === 0 || Math.abs(window.innerHeight - lastNeed) <= 3;
    const resize = fitted || (snap.settings.compact && view === "usage");
    lastNeed = need;
    invoke("fit_height", { height: need, resize });
  });
}

function renderUsage() {
  const el = $("view-usage");
  if (!snap.accounts.length) {
    el.innerHTML = `
      <div class="empty">
        <p>추가된 계정이 없습니다.</p>
        <div class="empty-actions">
          <button class="primary" data-action="login">Claude 계정 추가</button>
          <button data-action="login-chatgpt">ChatGPT 계정 추가</button>
        </div>
        <button class="link" data-action="settings">API 사용액 등 다른 방법으로 추가</button>
      </div>`;
  } else if (snap.settings.compact) {
    el.innerHTML = snap.accounts.map(compactRowHtml).join("");
  } else {
    el.innerHTML = snap.accounts.map(cardHtml).join("");
  }
  document.body.classList.toggle("compact", !!snap.settings.compact);
  tick();
}

function tick() {
  const now = Date.now();
  document.querySelectorAll("[data-reset]").forEach((e) => {
    const t = new Date(e.dataset.reset);
    e.textContent = `${fmtDuration(t - now)} 후 초기화 · ${fmtClock(t)}`;
  });
  document.querySelectorAll("[data-reset-at]").forEach((e) => {
    e.textContent = ` · ${fmtClock(new Date(e.dataset.resetAt))} 초기화`;
  });
  document.querySelectorAll("[data-tbar]").forEach((e) => {
    const left = new Date(e.dataset.tbar) - now;
    const elapsed = 1 - Math.max(0, Math.min(1, left / Number(e.dataset.win)));
    // 점이 길을 따라 깃발(초기화 시점)로 다가간다. 지나온 길은 실선, 남은 길은 점선.
    e.querySelector(".road .done").style.width = `${elapsed * 100}%`;
    e.querySelector(".road .dot").style.left = `${elapsed * 100}%`;
    e.querySelector(".journey").title = `구간의 ${Math.round(elapsed * 100)}% 지남 — 점이 깃발에 닿으면 초기화`;
    e.querySelector(".tleft b").textContent = left > 0 ? `${fmtDuration(left)} 남음` : "초기화 중";
    // 구간의 90% 이상 지났으면 "곧 초기화"로 강조
    e.classList.toggle("soon", elapsed >= 0.9);
  });
  document.querySelectorAll("[data-eta]").forEach((e) => {
    const t = new Date(e.dataset.eta);
    e.textContent = `이 속도면 ${fmtDuration(t - now)} 후 한도`;
    e.title = `예상 도달 시각 ${fmtClock(t)}`;
  });
  const times = Object.values(snap.statuses).map((s) => s.updated_at).filter(Boolean).sort();
  $("updated").textContent = times.length ? fmtAgo(times[times.length - 1]) : "";
}

// ---------- 설정 화면 ----------

function sourceText(a) {
  const plan = a.plan ? ` · ${a.plan}` : "";
  switch (a.source.kind) {
    case "claude_web": return `claude.ai 로그인${plan}`;
    case "claude_code": return `Claude Code · ${a.source.credentials_path}`;
    case "chatgpt_web": return `chatgpt.com 로그인 · Codex 한도${plan}`;
    case "anthropic_admin": return "Anthropic Admin 키 · 이번 달 API 사용액";
    case "openai_admin": return "OpenAI Admin 키 · 이번 달 API 사용액";
    default: return "";
  }
}

function renderSettings() {
  const list = $("account-list");
  list.innerHTML = snap.accounts.length
    ? snap.accounts
        .map(
          (a, i) => `
      <li data-id="${esc(a.id)}">
        <div class="top">
          <input type="text" value="${esc(a.label)}" data-action="rename" aria-label="표시 이름">
          <button data-action="up" ${i === 0 ? "disabled" : ""} title="위로">↑</button>
          <button data-action="down" ${i === snap.accounts.length - 1 ? "disabled" : ""} title="아래로">↓</button>
          <button data-action="remove" class="danger" title="삭제">삭제</button>
        </div>
        <div class="src" title="${esc(sourceText(a))}">${esc(sourceText(a))}</div>
        ${isApi(a) ? `
        <label class="budget">월 예산 $
          <input type="number" min="0" step="1" value="${a.budget_usd ?? ""}" placeholder="없음" data-action="budget">
        </label>` : ""}
      </li>`,
        )
        .join("")
    : `<li class="none">아직 계정이 없습니다.</li>`;

  const s = snap.settings;
  // 입력 중인 칸은 덮어쓰지 않는다.
  const setIfIdle = (id, v) => { if (document.activeElement !== $(id)) $(id).value = v; };
  setIfIdle("set-interval", String(s.interval_secs));
  setIfIdle("set-warn", s.warn_percent);
  setIfIdle("set-crit", s.crit_percent);
  $("set-notify").checked = s.notifications;
  $("set-notify-reset").checked = s.notify_reset;
  $("set-tray-percent").checked = s.tray_percent ?? true;
  $("set-compact").checked = !!s.compact;
  $("set-shortcut").value = prettyShortcut(s.shortcut ?? "");
  $("set-ontop").checked = s.always_on_top;
  $("set-autostart").checked = snap.autostart;
}

/** "CommandOrControl+Shift+U" → "Ctrl+Shift+U" (macOS는 "Cmd") */
function prettyShortcut(s) {
  const mac = /Mac/i.test(navigator.platform);
  return s.replace(/CommandOrControl|CmdOrCtrl/gi, mac ? "Cmd" : "Ctrl");
}

function render() {
  $("view-usage").hidden = view !== "usage";
  $("view-settings").hidden = view !== "settings";
  if (view === "usage") renderUsage();
  else renderSettings();
  fitWindow();
  const sc = snap.settings.shortcut;
  $("btn-hide").title = `숨기기 (트레이 아이콘${sc ? ` 또는 ${prettyShortcut(sc)}` : ""}로 다시 열기)`;
  $("btn-compact").title = snap.settings.compact ? "전체 보기로 전환" : "미니 모드로 전환";
}

function show(v) {
  view = v;
  render();
}

// ---------- 동작 ----------

let toastTimer;
function toast(msg, isError = false) {
  const t = $("toast");
  t.textContent = msg;
  t.className = isError ? "error" : "";
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), isError ? 6000 : 3000);
}

async function call(cmd, args) {
  try {
    return await invoke(cmd, args);
  } catch (e) {
    toast(String(e), true);
    throw e;
  }
}

async function startLogin(cmd = "start_claude_login") {
  await call(cmd);
  toast("로그인 창이 열렸습니다. 로그인이 끝나면 자동으로 추가됩니다.");
}

function saveSettings() {
  const s = {
    ...snap.settings,
    interval_secs: Number($("set-interval").value),
    warn_percent: Number($("set-warn").value) || 80,
    crit_percent: Number($("set-crit").value) || 95,
    notifications: $("set-notify").checked,
    notify_reset: $("set-notify-reset").checked,
    tray_percent: $("set-tray-percent").checked,
    compact: $("set-compact").checked,
    always_on_top: $("set-ontop").checked,
  };
  call("update_settings", { settings: s });
}

document.addEventListener("click", async (e) => {
  const btn = e.target.closest("[data-action]");
  if (!btn || btn.tagName === "INPUT") return;
  const id = btn.closest("li[data-id]")?.dataset.id;
  switch (btn.dataset.action) {
    case "login": return startLogin();
    case "login-chatgpt": return startLogin("start_chatgpt_login");
    case "settings": return show("settings");
    case "up": return call("move_account", { id, delta: -1 });
    case "down": return call("move_account", { id, delta: 1 });
    case "remove": {
      const a = snap.accounts.find((x) => x.id === id);
      if (confirm(`'${a?.label}' 계정을 삭제할까요? 저장된 로그인 정보도 지워집니다.`)) await call("remove_account", { id });
      return;
    }
  }
});

document.addEventListener("change", (e) => {
  const id = e.target.closest("li[data-id]")?.dataset.id;
  if (e.target.matches('[data-action="rename"]')) {
    call("rename_account", { id, label: e.target.value });
  } else if (e.target.matches('[data-action="budget"]')) {
    const v = Number(e.target.value);
    call("set_budget", { id, budget: v > 0 ? v : null });
  }
});

$("btn-refresh").addEventListener("click", async () => {
  const b = $("btn-refresh");
  b.classList.add("spinning");
  await call("refresh_now");
  setTimeout(() => b.classList.remove("spinning"), 1500);
});
$("btn-settings").addEventListener("click", () => show(view === "settings" ? "usage" : "settings"));
$("btn-back").addEventListener("click", () => show("usage"));
$("btn-hide").addEventListener("click", () => window.__TAURI__.window.getCurrentWindow().hide());
$("btn-quit").addEventListener("click", () => call("quit_app"));
$("btn-login").addEventListener("click", () => startLogin());
$("btn-login-chatgpt").addEventListener("click", () => startLogin("start_chatgpt_login"));

$("btn-add-api").addEventListener("click", async () => {
  const b = $("btn-add-api");
  const budget = Number($("api-budget").value);
  b.disabled = true;
  b.textContent = "확인 중…";
  try {
    await call("add_api_key", {
      provider: $("api-provider").value,
      key: $("api-key").value,
      label: $("api-label").value,
      budget: budget > 0 ? budget : null,
    });
    $("api-key").value = "";
    $("api-label").value = "";
    $("api-budget").value = "";
    toast("API 계정을 추가했습니다.");
  } finally {
    b.disabled = false;
    b.textContent = "추가";
  }
});

$("btn-add-key").addEventListener("click", async () => {
  const key = $("session-key").value.trim();
  if (!key) return;
  const b = $("btn-add-key");
  b.disabled = true;
  try {
    const n = await call("add_session_key", { key });
    $("session-key").value = "";
    toast(n > 0 ? `계정 ${n}개를 추가했습니다.` : "기존 계정의 로그인 정보를 갱신했습니다.");
  } finally {
    b.disabled = false;
  }
});

$("btn-add-cc").addEventListener("click", async () => {
  await call("add_claude_code", { path: $("cc-path").value, label: $("cc-label").value });
  $("cc-label").value = "";
  toast("Claude Code 계정을 추가했습니다.");
});

$("btn-compact").addEventListener("click", () => {
  call("update_settings", { settings: { ...snap.settings, compact: !snap.settings.compact } });
});

/** 단축키 칸: 누른 키 조합을 그대로 받아 저장한다. Backspace/Delete는 끄기. */
const MOD_KEYS = ["Control", "Shift", "Alt", "Meta"];
$("set-shortcut").addEventListener("keydown", (e) => {
  e.preventDefault();
  if (e.key === "Tab") return;
  if (e.key === "Backspace" || e.key === "Delete") {
    call("update_settings", { settings: { ...snap.settings, shortcut: "" } });
    return;
  }
  if (MOD_KEYS.includes(e.key)) return; // 조합 키만 누른 상태
  const mods = [];
  if (e.ctrlKey) mods.push("Ctrl");
  if (e.altKey) mods.push("Alt");
  if (e.shiftKey) mods.push("Shift");
  if (e.metaKey) mods.push("Super");
  if (!mods.length) {
    toast("Ctrl·Alt·Shift 중 하나 이상과 함께 눌러 주세요.", true);
    return;
  }
  // e.code: "KeyU" → "U", "Digit1" → "1", "F5" → "F5"
  const key = e.code.replace(/^Key/, "").replace(/^Digit/, "");
  const shortcut = [...mods, key].join("+");
  $("set-shortcut").value = prettyShortcut(shortcut);
  call("update_settings", { settings: { ...snap.settings, shortcut } })
    .then(() => toast(`단축키를 ${prettyShortcut(shortcut)}(으)로 바꿨습니다.`))
    .catch(() => ($("set-shortcut").value = prettyShortcut(snap.settings.shortcut ?? "")));
});

for (const id of ["set-interval", "set-warn", "set-crit", "set-notify", "set-notify-reset", "set-tray-percent", "set-compact", "set-ontop"]) {
  $(id).addEventListener("change", saveSettings);
}
$("set-autostart").addEventListener("change", (e) => call("set_autostart", { enabled: e.target.checked }));

// ---------- 시작 ----------

listen("snapshot", (e) => {
  snap = e.payload;
  render();
});

listen("login-result", (e) => {
  const r = e.payload;
  if (r.ok) {
    toast(r.added > 0 ? `계정 ${r.added}개를 추가했습니다.` : "로그인 정보를 갱신했습니다.");
    show("usage");
  } else if (!r.cancelled) {
    toast(r.error || "로그인에 실패했습니다.", true);
  }
});

// ---------- 앱 업데이트 ----------

async function checkUpdate(silent) {
  try {
    const info = await invoke("check_update");
    if (info) {
      $("update-text").textContent = `새 버전 ${info.version}이 있습니다`;
      $("update-bar").title = info.notes || "";
      $("update-bar").hidden = false;
      fitWindow();
    } else if (!silent) {
      toast("최신 버전입니다.");
    }
  } catch (e) {
    // 자동 확인 실패(오프라인, 아직 릴리스 없음 등)는 조용히 넘긴다
    if (!silent) toast(String(e), true);
  }
}

$("btn-check-update").addEventListener("click", () => checkUpdate(false));

$("btn-update").addEventListener("click", async () => {
  const b = $("btn-update");
  b.disabled = true;
  $("update-text").textContent = "내려받는 중…";
  try {
    // 설치에 성공하면 앱이 다시 시작된다. false면 이미 최신 버전이다.
    const installed = await invoke("install_update");
    if (installed === false) {
      $("update-bar").hidden = true;
      b.disabled = false;
      fitWindow();
      toast("이미 최신 버전입니다.");
    }
  } catch (e) {
    toast(String(e), true);
    $("update-text").textContent = "업데이트에 실패했습니다";
    b.disabled = false;
  }
});

listen("update-progress", (e) => {
  const { received, total } = e.payload;
  $("update-text").textContent = total
    ? `내려받는 중… ${Math.min(100, Math.round((received / total) * 100))}%`
    : `내려받는 중… ${(received / 1048576).toFixed(1)}MB`;
});

(async () => {
  $("cc-path").value = await invoke("default_claude_code_path");
  $("app-version").textContent = await invoke("app_version");
  snap = await invoke("get_snapshot");
  render();
  setInterval(tick, 1000);
  setTimeout(() => checkUpdate(true), 5000);
  setInterval(() => checkUpdate(true), 6 * HOUR);
})();
