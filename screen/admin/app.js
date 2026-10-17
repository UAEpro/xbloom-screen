// xBloom Screen — admin panel.
//
// A dependency-free single-page app served by the device itself (Raspberry Pi or ESP32-S3).
// It talks to the device's REST API (screen/crates/xbloom-app/src/api.rs). User content is
// always inserted as text, never as HTML.
//
// The panel can't start a brew — hot water only flows from the touchscreen — but it can stop one.

import { ICONS } from "/icons.js";
import { probeVideo, convertVideo, convertPhoto } from "/xbv.js";

// ── DOM helpers ─────────────────────────────────────────────────────────────────────────

/** Create an element. Props: class, style, on<event>, DOM properties, or attributes. */
function h(tag, props, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === "class") el.className = v;
    else if (k === "style") el.setAttribute("style", v);
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k in el && typeof v !== "string") el[k] = v;
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const c of kids.flat(Infinity)) {
    if (c == null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

/** A Lucide icon (static, trusted markup). */
function icon(name) {
  const s = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  s.setAttribute("viewBox", "0 0 24 24");
  s.setAttribute("fill", "none");
  s.setAttribute("stroke", "currentColor");
  s.setAttribute("stroke-width", "2");
  s.setAttribute("stroke-linecap", "round");
  s.setAttribute("stroke-linejoin", "round");
  s.setAttribute("aria-hidden", "true");
  s.innerHTML = ICONS[name] || "";
  return s;
}

const btn = (label, onclick, cls = "", ico = null, extra = {}) =>
  h("button", { class: `btn ${cls}`, type: "button", onclick, ...extra }, ico && icon(ico), label);

function toast(message, kind = "info") {
  const t = h("div", { class: `toast ${kind}` }, message);
  document.getElementById("toasts").append(t);
  setTimeout(() => t.remove(), kind === "error" ? 5200 : 3200);
}

function modal(build, { wide = false } = {}) {
  const back = h("div", { class: "modal-back" });
  const box = h("div", { class: `modal ${wide ? "wide" : ""}`, role: "dialog", "aria-modal": "true" });
  const close = () => {
    back.remove();
    document.removeEventListener("keydown", onKey);
  };
  const onKey = (e) => e.key === "Escape" && close();
  back.addEventListener("click", (e) => e.target === back && close());
  document.addEventListener("keydown", onKey);
  box.append(build(close));
  back.append(box);
  document.body.append(back);
  box.querySelector("input,select,textarea,button")?.focus();
  return close;
}

function confirmDialog(title, text, ok = "Delete", danger = true) {
  return new Promise((resolve) => {
    modal((close) =>
      h("div", { class: "stack" },
        h("h2", {}, title),
        h("p", { class: "muted", style: "margin:0" }, text),
        h("div", { class: "row", style: "justify-content:flex-end" },
          btn("Cancel", () => { close(); resolve(false); }, "secondary"),
          btn(ok, () => { close(); resolve(true); }, danger ? "danger" : ""))));
  });
}

const sw = (checked, onchange, label) =>
  h("label", { class: "switch", title: label }, h("input", { type: "checkbox", checked, onchange: (e) => onchange(e.target.checked), "aria-label": label }), h("span"));

function seg(options, value, onpick) {
  const el = h("div", { class: "seg", role: "radiogroup" });
  options.forEach(([v, label]) =>
    el.append(h("button", { type: "button", class: v === value ? "on" : "", role: "radio", "aria-checked": String(v === value), onclick: () => onpick(v) }, label)));
  return el;
}

const setting = (title, desc, control) =>
  h("div", { class: "setting" }, h("div", { class: "text" }, h("b", {}, title), desc && h("span", {}, desc)), control);

const fmtTime = (s) => `${Math.floor(s / 60)}:${String(Math.round(s % 60)).padStart(2, "0")}`;
const ratioText = (r) => (Math.abs(r - Math.round(r)) < 0.05 ? `1:${Math.round(r)}` : `1:${r.toFixed(1)}`);
const fmtDate = (ts) => new Date(ts * 1000).toLocaleString(undefined, { weekday: "short", day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" });
const fmtBytes = (b) => (b > 1e9 ? `${(b / 1e9).toFixed(1)} GB` : b > 1e6 ? `${(b / 1e6).toFixed(1)} MB` : `${Math.round(b / 1e3)} kB`);
const TINTS = ["#c8743a", "#b5523b", "#8a6a3d", "#3f8f6b", "#3a7cb8", "#7a5cc0"];

// ── API ─────────────────────────────────────────────────────────────────────────────────

class ApiError extends Error {
  constructor(message, status, data) {
    super(message);
    this.status = status;
    this.data = data;
  }
}

async function api(method, path, body) {
  const opts = { method, credentials: "same-origin", headers: { "X-XBloom": "1" } };
  if (body !== undefined) {
    if (typeof body === "string" || body instanceof Blob) opts.body = body;
    else {
      opts.body = JSON.stringify(body);
      opts.headers["Content-Type"] = "application/json";
    }
  }
  const res = await fetch(path, opts);
  const isJson = (res.headers.get("content-type") || "").includes("json");
  const data = isJson ? await res.json() : await res.text();
  if (res.status === 401 && !path.endsWith("/login") && !path.endsWith("/setup")) {
    S.authed = false;
    renderLogin();
  }
  if (!res.ok) throw new ApiError((data && data.error) || `HTTP ${res.status}`, res.status, data);
  return data;
}

/** POST a Blob with upload progress (fetch has none). */
function upload(path, blob, onProgress) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open("POST", path);
    xhr.setRequestHeader("X-XBloom", "1");
    xhr.upload.onprogress = (e) => e.lengthComputable && onProgress?.(e.loaded / e.total);
    xhr.onload = () => {
      let data = {};
      try { data = JSON.parse(xhr.responseText); } catch { /* not JSON */ }
      if (xhr.status >= 200 && xhr.status < 300) resolve(data);
      else reject(new ApiError(data.error || `HTTP ${xhr.status}`, xhr.status, data));
    };
    xhr.onerror = () => reject(new ApiError("network error — is the screen still on?", 0));
    xhr.send(blob);
  });
}

// ── state, theme, boot ──────────────────────────────────────────────────────────────────

const S = { info: null, settings: null, status: null, recipes: [], presets: null, media: [], history: [], authed: false, scanning: false };
const $app = document.getElementById("app");

function applyTheme() {
  const pref = localStorage.getItem("xb-theme") || "system";
  const dark = pref === "dark" || (pref === "system" && matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.dataset.theme = dark ? "dark" : "light";
}
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", applyTheme);

function setAccent(hex) {
  if (/^#[0-9a-f]{6}$/i.test(hex || "")) document.documentElement.style.setProperty("--accent", hex);
}

async function boot() {
  applyTheme();
  try {
    S.info = await api("GET", "/api/info");
  } catch {
    $app.replaceChildren(h("div", { class: "login" }, h("div", { class: "card pad" }, h("h2", {}, "Can't reach the screen"), h("p", { class: "muted" }, "Check that it's on and on the same network, then reload."))));
    return;
  }
  setAccent(S.info.accent);
  document.title = `${S.info.device_name} · Admin`;
  if (!S.info.authed) return renderLogin();
  try {
    await loadAll();
    S.authed = true;
    renderShell();
  } catch (e) {
    if (e.status !== 401) toast(e.message, "error");
    renderLogin();
  }
}

async function loadAll() {
  const [settings, status, recipes, presets, media, history] = await Promise.all([
    api("GET", "/api/settings"), api("GET", "/api/status"), api("GET", "/api/recipes"),
    api("GET", "/api/presets"), api("GET", "/api/media"), api("GET", "/api/history"),
  ]);
  Object.assign(S, { settings, status, recipes, presets, media, history });
  setAccent(settings.appearance.accent);
}

// ── login / first-run ───────────────────────────────────────────────────────────────────

function renderLogin() {
  clearInterval(S.poller);
  const setup = S.info?.setup_required;
  const pin = h("input", { class: "input pin-input", type: "password", inputmode: "numeric", autocomplete: setup ? "new-password" : "current-password", maxlength: "8", placeholder: "••••", "aria-label": "PIN" });
  const pin2 = setup ? h("input", { class: "input pin-input", type: "password", inputmode: "numeric", autocomplete: "new-password", maxlength: "8", placeholder: "••••", "aria-label": "Confirm PIN" }) : null;
  const err = h("p", { class: "small", style: "color:var(--danger);margin:0;min-height:20px" });
  const go = btn(setup ? "Create PIN" : "Sign in", submit, "block");
  async function submit(e) {
    e?.preventDefault();
    err.textContent = "";
    if (!/^\d{4,8}$/.test(pin.value)) return (err.textContent = "Use 4 to 8 digits.");
    if (setup && pin.value !== pin2.value) return (err.textContent = "The PINs don't match.");
    go.disabled = true;
    try {
      await api("POST", setup ? "/api/setup" : "/api/login", { pin: pin.value });
      S.info.setup_required = false;
      await loadAll();
      S.authed = true;
      renderShell();
    } catch (ex) {
      err.textContent = ex.message;
      pin.value = "";
      pin.focus();
    } finally {
      go.disabled = false;
    }
  }
  $app.replaceChildren(
    h("div", { class: "login" },
      h("form", { class: "card", onsubmit: submit },
        h("div", { class: "stack" },
          h("div", { class: "logo" }, icon("coffee")),
          h("div", {},
            h("h1", {}, S.info?.device_name || "xBloom Screen"),
            h("p", { class: "muted", style: "margin:6px 0 0" }, setup
              ? "Create the admin PIN. It protects the screen's settings and this panel."
              : "Sign in with the admin PIN set on the screen.")),
          pin, pin2, err, go,
          h("p", { class: "faint small", style: "margin:0" }, "Unofficial — not affiliated with xBloom.")))));
  pin.focus();
}

// ── shell + router ──────────────────────────────────────────────────────────────────────

const NAV = [
  ["#/", "Home", "layout-dashboard"],
  ["#/recipes", "Recipes", "book-open"],
  ["#/presets", "Dial", "disc-3"],
  ["#/screensaver", "Screensaver", "film"],
  ["#/history", "History", "history"],
  ["#/settings", "Settings", "settings"],
];

function machinePill() {
  const m = S.status?.machine || {};
  const cls = m.link === "connected" ? "ok" : m.link === "connecting" ? "busy" : "";
  const label = m.link === "connected" ? (m.name || "Connected") + (m.simulator ? " · demo" : "") : m.link === "connecting" ? "Connecting…" : "Machine offline";
  return h("span", { class: `pill ${cls}`, id: "machine-pill" }, h("span", { class: "dot" }), label);
}

function renderShell() {
  const nav = (cls) => NAV.map(([href, label, ico]) => h("a", { class: `navlink ${cls}`, href, "data-href": href }, icon(ico), label));
  $app.replaceChildren(
    h("div", { class: "shell" },
      h("aside", { class: "sidebar" },
        h("div", { class: "brand" }, h("div", { class: "logo" }, icon("coffee")), h("div", {}, h("b", {}, S.info.device_name), h("div", { class: "faint small" }, "Admin"))),
        nav(""),
        h("div", { class: "spacer" }),
        btn("Sign out", signOut, "ghost", "log-out")),
      h("div", {},
        h("header", { class: "topbar" },
          h("div", { class: "logo" }, icon("coffee")),
          h("div", { class: "title grow ellipsis" }, S.info.device_name),
          machinePill(),
          h("button", { class: "btn icon ghost", type: "button", title: "Sign out", "aria-label": "Sign out", onclick: signOut }, icon("log-out"))),
        h("main", { id: "main" })),
      h("nav", { class: "tabbar" }, nav(""))));
  route();
  clearInterval(S.poller);
  S.poller = setInterval(poll, 2000);
}

async function signOut() {
  try { await api("POST", "/api/logout"); } catch { /* ignore */ }
  S.authed = false;
  renderLogin();
}

const ROUTES = [
  [/^#?\/?$/, pageHome],
  [/^#\/recipes$/, pageRecipes],
  [/^#\/recipes\/new$/, () => pageEditor(null)],
  [/^#\/recipes\/([\w-]+)$/, (m) => pageEditor(m[1])],
  [/^#\/presets$/, pagePresets],
  [/^#\/screensaver$/, pageScreensaver],
  [/^#\/history$/, pageHistory],
  [/^#\/settings$/, pageSettings],
];

function route() {
  if (!S.authed) return;
  const hash = location.hash || "#/";
  const main = document.getElementById("main");
  if (!main) return;
  for (const [re, page] of ROUTES) {
    const m = hash.match(re);
    if (m) {
      S.page = page;
      main.replaceChildren(page(m));
      document.querySelectorAll(".navlink").forEach((a) => {
        const href = a.dataset.href;
        a.classList.toggle("active", href === "#/" ? hash === "#/" || hash === "" : hash.startsWith(href));
      });
      window.scrollTo(0, 0);
      return;
    }
  }
  location.hash = "#/";
}
window.addEventListener("hashchange", route);

/** Re-render the current page in place (after data changes). */
function rerender() {
  const main = document.getElementById("main");
  const hash = location.hash || "#/";
  for (const [re, page] of ROUTES) {
    const m = hash.match(re);
    if (m && main) return main.replaceChildren(page(m));
  }
}

async function poll() {
  if (!S.authed || document.hidden) return;
  try {
    S.status = await api("GET", "/api/status");
  } catch {
    return;
  }
  document.getElementById("machine-pill")?.replaceWith(machinePill());
  if (document.getElementById("preset-status")) {
    try { S.presets = await api("GET", "/api/presets"); } catch { /* keep the old one */ }
  }
  const live = document.getElementById("live");
  if (live) live.replaceWith(liveCards());
  const pstatus = document.getElementById("preset-status");
  if (pstatus) pstatus.replaceWith(presetStatus());
}

// ── home ────────────────────────────────────────────────────────────────────────────────

function greeting() {
  const hr = new Date().getHours();
  return hr < 5 ? "Good evening" : hr < 12 ? "Good morning" : hr < 17 ? "Good afternoon" : "Good evening";
}

function machineCard() {
  const m = S.status.machine;
  const ok = m.link === "connected";
  const scan = h("div", { class: "stack", style: "gap:0" });
  for (const d of m.scan_results || []) {
    scan.append(h("div", { class: "list-item" },
      icon("bluetooth"),
      h("div", { class: "grow" }, h("b", {}, d.name || d.address), h("div", { class: "faint small" }, `${d.address}${d.rssi ? ` · ${d.rssi} dBm` : ""}`)),
      btn("Pair", async () => { await api("POST", "/api/machine/connect", { address: d.address }); toast("Connecting…"); }, "small")));
  }
  const detail = m.busy || (ok ? `Connected · ${(m.machine_state || "ready").replace(/_/g, " ")}` : m.link === "connecting" ? "Connecting…" : m.error || "Disconnected");
  return h("div", { class: "card pad stack" },
    h("div", { class: "row" },
      h("div", { class: `machine-icon ${ok ? "ok" : ""}` }, icon(ok ? "bluetooth-connected" : "bluetooth-off")),
      h("div", { class: "grow" },
        h("h2", { class: "ellipsis" }, m.name || (S.settings.machine.address ? "xBloom" : "No machine paired"), m.simulator && h("span", { class: "badge", style: "margin-left:8px" }, "demo")),
        h("div", { class: "muted small" }, detail))),
    h("div", { class: "row wrap" },
      ok ? btn("Disconnect", async () => { await api("POST", "/api/machine/disconnect"); toast("Disconnected — the phone app can connect now"); }, "outline small")
        : btn("Connect", async () => { await api("POST", "/api/machine/connect"); toast("Connecting…"); }, "small", null, { disabled: !(S.settings.machine.address || m.simulator) }),
      !m.simulator && btn(m.scanning ? "Scanning…" : "Scan for machines", async () => { await api("POST", "/api/machine/scan"); }, "secondary small", "refresh-cw", { disabled: m.scanning })),
    scan);
}

function brewCard() {
  const b = S.status.machine.brew;
  if (!b) {
    return h("div", { class: "card pad stack" },
      h("div", { class: "row" }, h("div", { class: "machine-icon" }, icon("coffee")), h("div", { class: "grow" }, h("h2", {}, "Nothing brewing"), h("div", { class: "muted small" }, "Brews start at the screen — the panel never pours water."))));
  }
  const pct = Math.round((b.progress || 0) * 100);
  return h("div", { class: "card pad stack" },
    h("div", { class: "row" },
      h("div", { class: `machine-icon ${b.finished && b.outcome === "completed" ? "ok" : ""}` }, icon(b.finished ? (b.outcome === "completed" ? "check" : "triangle-alert") : "droplet")),
      h("div", { class: "grow" }, h("h2", { class: "ellipsis" }, b.recipe_name), h("div", { class: "muted small" }, b.message))),
    h("div", { class: "progress" }, h("div", { style: `width:${pct}%` })),
    h("div", { class: "row small muted" },
      h("span", { class: "grow" }, `${Math.round(b.water_g)} of ${Math.round(b.target_g)} g water · ${Math.round(b.coffee_g)} g in cup`),
      h("span", {}, fmtTime(b.elapsed_s))),
    !b.finished && btn("Stop brew", async () => {
      if (await confirmDialog("Stop the brew?", "The machine stops pouring and returns to idle.", "Stop brew")) {
        await api("POST", "/api/brew/cancel");
        toast("Stopping…");
      }
    }, "danger", "x"));
}

function liveCards() {
  return h("div", { class: "grid two", id: "live" }, machineCard(), brewCard());
}

function pageHome() {
  const done = S.history.filter((e) => e.outcome === "completed").length;
  const p = S.presets;
  return h("div", {},
    h("div", { class: "page-head" }, h("div", { class: "grow" }, h("h1", {}, greeting()), h("p", { class: "muted" }, `${S.info.device_name} · ${S.info.platform === "esp32s3" ? "ESP32-S3" : S.info.platform === "raspberry-pi" ? "Raspberry Pi" : "Linux"} · v${S.info.version}`))),
    h("div", { class: "stack" },
      liveCards(),
      h("div", { class: "stats" },
        h("a", { class: "card stat", href: "#/recipes", style: "text-decoration:none;color:inherit" }, h("div", { class: "caption" }, "Recipes"), h("div", { class: "v" }, S.recipes.length)),
        h("a", { class: "card stat", href: "#/history", style: "text-decoration:none;color:inherit" }, h("div", { class: "caption" }, "Brews"), h("div", { class: "v" }, done)),
        h("a", { class: "card stat", href: "#/presets", style: "text-decoration:none;color:inherit" }, h("div", { class: "caption" }, "Dial presets"), h("div", { class: "v", style: "font-size:18px;padding-top:6px" }, !p.slots.every(Boolean) ? "Not set" : p.dirty ? "Not saved" : "On machine"))),
      h("div", { class: "card pad" },
        h("h3", { style: "margin-bottom:12px" }, "Quick actions"),
        h("div", { class: "row wrap" },
          h("a", { class: "btn", href: "#/recipes/new" }, icon("plus"), "New recipe"),
          btn("Import recipes", importDialog, "secondary", "upload"),
          h("a", { class: "btn secondary", href: "#/screensaver" }, icon("film"), "Screensaver"),
          btn("Lock the screen", async () => { await api("POST", "/api/device/lock"); toast("Screen locked"); }, "secondary", "lock")))));
}

// ── recipes ─────────────────────────────────────────────────────────────────────────────

function kindOf(r) {
  if (r.grind === 0) return "no-grind";
  if (r.ice_g != null || r.hot_water_ml != null || /iced/i.test(r.name)) return "iced";
  return "filter";
}
const totalMl = (r) => r.pours.reduce((s, p) => s + Number(p.ml || 0), 0);
const ratioOf = (r) => (r.ratio != null ? Number(r.ratio) : r.dose_g ? Math.round((totalMl(r) / r.dose_g) * 10) / 10 : 0);

async function reloadRecipes() {
  S.recipes = await api("GET", "/api/recipes");
}

function pageRecipes() {
  const q = h("input", { class: "input", type: "search", placeholder: "Search recipes", "aria-label": "Search recipes" });
  const list = h("div", { class: "card", style: "overflow:hidden" });
  const draw = () => {
    const term = q.value.trim().toLowerCase();
    const items = S.recipes.filter((s) => !term || s.recipe.name.toLowerCase().includes(term) || (s.recipe.dripper || "").toLowerCase().includes(term));
    list.replaceChildren(...(items.length ? items.map(recipeRow) : [h("div", { class: "empty" }, icon("book-open"), h("div", {}, S.recipes.length ? "No matches." : "No recipes yet — create one or import YAML from the xbloom CLI."))]));
  };
  q.addEventListener("input", draw);
  draw();
  return h("div", {},
    h("div", { class: "page-head" },
      h("div", { class: "grow" }, h("h1", {}, "Recipes"), h("p", { class: "muted" }, "Favourites appear first on the screen. Hidden recipes stay here but not on the screen.")),
      h("div", { class: "row wrap" },
        h("a", { class: "btn ghost", href: "/api/recipes/export.json", download: "xbloom-recipes.json" }, icon("download"), "Export all"),
        btn("Import", importDialog, "secondary", "upload"),
        h("a", { class: "btn", href: "#/recipes/new" }, icon("plus"), "New recipe"))),
    h("div", { class: "stack" }, q, list));
}

function recipeRow(s, i) {
  const r = s.recipe;
  const tint = s.meta.color || TINTS[i % TINTS.length];
  const kind = kindOf(r);
  const star = h("button", { class: `star ${s.meta.favorite ? "on" : ""}`, type: "button", "aria-label": "Favourite", onclick: async (e) => {
    e.stopPropagation();
    const meta = { ...s.meta, favorite: !s.meta.favorite };
    try {
      await api("PUT", `/api/recipes/${s.id}`, { recipe: r, meta });
      await reloadRecipes();
      rerender();
    } catch (ex) { toast(ex.message, "error"); }
  } }, icon("star"));
  return h("div", { class: "recipe-row", onclick: () => (location.hash = `#/recipes/${s.id}`) },
    h("div", { class: "chipdot", style: `background:${tint}` }, icon(kind === "iced" ? "snowflake" : kind === "no-grind" ? "bean" : "droplet")),
    h("div", { class: "grow" },
      h("div", { class: "row", style: "gap:8px" }, h("b", { class: "ellipsis" }, r.name), s.meta.hidden && h("span", { class: "badge" }, "hidden"), kind !== "filter" && h("span", { class: "badge" }, kind)),
      h("div", { class: "meta" },
        h("span", {}, `${r.dose_g} g · ${ratioText(ratioOf(r))}`),
        h("span", {}, r.grind === 0 ? "pre-ground" : `grind ${r.grind}`),
        h("span", {}, `${totalMl(r)} ml · ${r.pours.length} pours`),
        r.dripper && h("span", {}, r.dripper))),
    star,
    h("span", { class: "faint" }, icon("chevron-right")));
}

function importDialog() {
  modal((close) => {
    const area = h("textarea", { class: "input", placeholder: "Paste YAML or JSON — one recipe, a list, or an export file", rows: 8 });
    const file = h("input", { type: "file", accept: ".yaml,.yml,.json,text/yaml,application/json,text/plain", class: "input" });
    const out = h("div");
    file.addEventListener("change", async () => {
      if (file.files[0]) area.value = await file.files[0].text();
    });
    const go = btn("Import", async () => {
      if (!area.value.trim()) return;
      go.disabled = true;
      try {
        const r = await api("POST", "/api/recipes/import", area.value);
        await reloadRecipes();
        out.replaceChildren(h("div", { class: `validation ${r.errors.length ? "bad" : "ok"}` },
          `Imported ${r.imported} recipe${r.imported === 1 ? "" : "s"}.`,
          r.errors.map((e) => h("div", { class: "small" }, e))));
        if (!r.errors.length) {
          toast(`Imported ${r.imported} recipe${r.imported === 1 ? "" : "s"}`, "success");
          close();
        }
        rerender();
      } catch (ex) {
        out.replaceChildren(h("div", { class: "validation bad" }, ex.message));
      } finally {
        go.disabled = false;
      }
    });
    return h("div", { class: "stack" },
      h("h2", {}, "Import recipes"),
      h("p", { class: "muted small", style: "margin:0" }, "Accepts the xbloom CLI's recipe YAML, JSON, or a recipe export from this panel. Each recipe is validated against the machine's limits."),
      file, area, out,
      h("div", { class: "row", style: "justify-content:flex-end" }, btn("Cancel", close, "secondary"), go));
  });
}

// ── recipe editor ───────────────────────────────────────────────────────────────────────

const PATTERNS = [["spiral", "Spiral"], ["ring", "Ring"], ["center", "Center"]];
const blankPour = (i) => ({ label: i === 0 ? "Bloom" : `Pour ${i}`, ml: i === 0 ? 45 : 100, temp_c: 92, pattern: "spiral", agitation: i === 0, pause_s: i === 0 ? 35 : 5, rpm: 100, flow_ml_s: 3.0 });

function numField(label, value, onchange, opts = {}) {
  return h("label", { class: "field" }, label,
    h("input", { class: "input num", type: "number", inputmode: "decimal", value: value ?? "", min: opts.min, max: opts.max, step: opts.step || 1, placeholder: opts.placeholder || "", oninput: (e) => onchange(e.target.value === "" ? null : Number(e.target.value)) }));
}
function textField(label, value, onchange, opts = {}) {
  return h("label", { class: "field" }, label,
    h("input", { class: "input", type: "text", value: value ?? "", placeholder: opts.placeholder || "", maxlength: opts.max || 80, oninput: (e) => onchange(e.target.value) }));
}

function pageEditor(id) {
  const stored = id ? S.recipes.find((s) => s.id === id) : null;
  if (id && !stored) {
    return h("div", { class: "empty" }, h("p", {}, "That recipe doesn't exist any more."), h("a", { class: "btn", href: "#/recipes" }, "Back to recipes"));
  }
  const r = structuredClone(stored ? stored.recipe : { name: "", dose_g: 15, grind: 55, stage_temps: [110, 90], pours: [blankPour(0), blankPour(1), blankPour(2)] });
  const meta = structuredClone(stored ? stored.meta : { favorite: false, hidden: false, color: null });
  r.pours = r.pours.map((p) => ({ label: p.label ?? "", ...p }));

  const summary = h("div", { class: "row wrap small muted" });
  const status = h("div", { class: "validation ok" }, "Checking…");
  const pourBox = h("div", { class: "pours" });
  let timer;
  const clean = () => {
    const out = { ...r, pours: r.pours.map((p) => ({ ...p, label: p.label?.trim() ? p.label.trim() : undefined })) };
    for (const k of ["ratio", "dripper", "kind", "water_ml", "hot_water_ml", "ice_g", "time", "note"]) {
      if (out[k] === "" || out[k] == null) delete out[k];
    }
    return out;
  };
  const validate = () => {
    clearTimeout(timer);
    summary.replaceChildren(
      h("span", {}, `Σ pours ${totalMl(r)} ml`),
      h("span", {}, `ratio ${ratioText(ratioOf(r))}`),
      h("span", {}, r.grind === 0 ? "pre-ground (grinder off)" : `grind ${r.grind}`));
    timer = setTimeout(async () => {
      try {
        const v = await api("POST", "/api/recipes/validate", { recipe: clean() });
        status.className = `validation ${v.ok ? "ok" : "bad"}`;
        status.textContent = v.ok ? `Ready to brew — ${v.total_water_ml} ml at ${ratioText(v.ratio)}.` : v.error;
      } catch (ex) {
        status.className = "validation bad";
        status.textContent = ex.message;
      }
    }, 250);
  };
  const set = (k) => (v) => { r[k] = v; validate(); };

  const drawPours = () => {
    pourBox.replaceChildren(...r.pours.map((p, i) => {
      const setp = (k) => (v) => { p[k] = v; validate(); };
      const agit = sw(!!p.agitation, (v) => { p.agitation = v; validate(); }, "Agitation");
      return h("div", { class: "pour" },
        h("div", { class: "head" },
          h("span", { class: "idx" }, i + 1),
          h("input", { class: "input grow", value: p.label || "", placeholder: i === 0 ? "Bloom" : `Pour ${i}`, "aria-label": "Label", oninput: (e) => (p.label = e.target.value) }),
          btn("", () => { if (i > 0) { [r.pours[i - 1], r.pours[i]] = [r.pours[i], r.pours[i - 1]]; drawPours(); validate(); } }, "ghost icon", "arrow-up", { title: "Move up", "aria-label": "Move up", disabled: i === 0 }),
          btn("", () => { if (i < r.pours.length - 1) { [r.pours[i + 1], r.pours[i]] = [r.pours[i], r.pours[i + 1]]; drawPours(); validate(); } }, "ghost icon", "arrow-down", { title: "Move down", "aria-label": "Move down", disabled: i === r.pours.length - 1 }),
          btn("", () => { r.pours.splice(i, 1); drawPours(); validate(); }, "ghost icon", "trash-2", { title: "Remove pour", "aria-label": "Remove pour", disabled: r.pours.length <= 2 })),
        numField("Water (ml)", p.ml, setp("ml"), { min: 1, max: 4000 }),
        numField("Temp (°C)", p.temp_c, setp("temp_c"), { min: 40, max: 95 }),
        h("label", { class: "field" }, "Pattern",
          h("select", { class: "input", onchange: (e) => { p.pattern = e.target.value; if (p.pattern !== "spiral") { p.agitation = false; } if (p.pattern === "center") p.rpm = 0; else if (!p.rpm) p.rpm = 100; drawPours(); validate(); } },
            PATTERNS.map(([v, l]) => h("option", { value: v, selected: p.pattern === v }, l)))),
        numField("Pause (s)", p.pause_s, setp("pause_s"), { min: 0, max: 255 }),
        numField("RPM", p.rpm, setp("rpm"), { min: 0, max: 120, step: 10 }),
        numField("Flow (ml/s)", p.flow_ml_s, setp("flow_ml_s"), { min: 3, max: 3.5, step: 0.1 }),
        h("label", { class: "field" }, "Agitation", h("div", { style: "height:44px;display:flex;align-items:center" }, p.pattern === "spiral" ? agit : h("span", { class: "faint small" }, "spiral only"))));
    }));
  };

  const colors = h("div", { class: "swatches" });
  const drawColors = () => colors.replaceChildren(
    h("button", { type: "button", class: `swatch ${!meta.color ? "on" : ""}`, title: "Automatic", style: "background:conic-gradient(#c8743a,#b5523b,#3a7cb8,#3f8f6b,#c8743a)", onclick: () => { meta.color = null; drawColors(); } }),
    ...TINTS.concat(["#2f2a26", "#c0567c"]).map((c) => h("button", { type: "button", class: `swatch ${meta.color === c ? "on" : ""}`, style: `background:${c}`, title: c, onclick: () => { meta.color = c; drawColors(); } })));
  drawColors();

  const save = btn("Save", async () => {
    save.disabled = true;
    try {
      const body = { recipe: clean(), meta };
      if (stored) await api("PUT", `/api/recipes/${stored.id}`, body);
      else await api("POST", "/api/recipes", body);
      await reloadRecipes();
      toast(`Saved “${r.name || "recipe"}”`, "success");
      location.hash = "#/recipes";
    } catch (ex) {
      toast(ex.message, "error");
    } finally {
      save.disabled = false;
    }
  }, "", "check");

  drawPours();
  validate();
  return h("div", {},
    h("div", { class: "page-head" },
      h("a", { class: "btn icon ghost", href: "#/recipes", "aria-label": "Back" }, icon("chevron-left")),
      h("div", { class: "grow" }, h("h1", {}, stored ? stored.recipe.name : "New recipe"), stored && h("p", { class: "muted small" }, `Brewed ${stored.meta.brews} time${stored.meta.brews === 1 ? "" : "s"}`)),
      h("div", { class: "row wrap" },
        stored && btn("Duplicate", async () => { await api("POST", `/api/recipes/${stored.id}/duplicate`); await reloadRecipes(); toast("Duplicated", "success"); location.hash = "#/recipes"; }, "ghost", "copy"),
        stored && h("a", { class: "btn ghost", href: `/api/recipes/${stored.id}/export.yaml`, download: "" }, icon("file-text"), "YAML"),
        stored && btn("Delete", async () => {
          if (await confirmDialog(`Delete “${stored.recipe.name}”?`, "It's removed from the screen too. Any dial slot using it is cleared.")) {
            await api("DELETE", `/api/recipes/${stored.id}`);
            await reloadRecipes();
            toast("Deleted");
            location.hash = "#/recipes";
          }
        }, "ghost", "trash-2"),
        save)),
    h("div", { class: "stack" },
      status,
      h("div", { class: "card pad stack" },
        h("h3", {}, "Recipe"),
        h("div", { class: "grid two" },
          textField("Name", r.name, set("name"), { placeholder: "Ethiopia Natural" }),
          textField("Dripper", r.dripper, set("dripper"), { placeholder: "Omni" })),
        h("div", { class: "grid three" },
          numField("Dose (g)", r.dose_g, set("dose_g"), { min: 1, max: 18 }),
          numField("Grind (1–80, 0 = pre-ground)", r.grind, set("grind"), { min: 0, max: 80 }),
          numField("Ratio (optional, 1:x)", r.ratio, set("ratio"), { min: 1, max: 30, step: 0.5, placeholder: "auto" })),
        h("div", { class: "grid three" },
          textField("Time", r.time, set("time"), { placeholder: "~2:30" }),
          numField("Stage temp 1 (°C)", r.stage_temps[0], (v) => { r.stage_temps[0] = v ?? 110; validate(); }, { min: 40, max: 130 }),
          numField("Stage temp 2 (°C)", r.stage_temps[1], (v) => { r.stage_temps[1] = v ?? 90; validate(); }, { min: 40, max: 130 })),
        h("label", { class: "field" }, "Notes", h("textarea", { class: "input", rows: 3, style: "font-family:inherit;font-size:15px;min-height:80px", maxlength: 400, oninput: (e) => (r.note = e.target.value) }, r.note || "")),
        summary),
      h("div", { class: "card pad stack" },
        h("div", { class: "row" }, h("h3", { class: "grow" }, "Pours"), btn("Add pour", () => { r.pours.push(blankPour(r.pours.length)); drawPours(); validate(); }, "secondary small", "plus")),
        pourBox),
      h("div", { class: "card pad" },
        h("h3", { style: "margin-bottom:6px" }, "On the screen"),
        setting("Card colour", null, colors),
        setting("Favourite", "Shown first.", sw(meta.favorite, (v) => (meta.favorite = v), "Favourite")),
        setting("Hidden", "Keep it here but don't show it on the screen.", sw(meta.hidden, (v) => (meta.hidden = v), "Hidden")))));
}

// ── dial presets ────────────────────────────────────────────────────────────────────────

function presetStatus() {
  const p = S.presets;
  const busy = S.status?.machine?.busy;
  let text;
  let cls = "pill";
  if (busy) { text = busy; cls = "pill busy"; }
  else if (!p.slots.every(Boolean)) { text = "Assign all three slots"; cls = "pill warn"; }
  else if (p.dirty) { text = "Not on the machine yet"; cls = "pill warn"; }
  else { text = `On the machine since ${fmtDate(p.pushed_at)}`; cls = "pill ok"; }
  return h("span", { class: cls, id: "preset-status" }, h("span", { class: "dot" }), text);
}

function pagePresets() {
  const p = S.presets;
  const save = async (slots, scale) => {
    try {
      await api("PUT", "/api/presets", { slots, scale });
      S.presets = await api("GET", "/api/presets");
      rerender();
    } catch (ex) { toast(ex.message, "error"); }
  };
  const slotCard = (i) => {
    const sel = h("select", { class: "input", onchange: (e) => { const slots = [...p.slots]; slots[i] = e.target.value || null; save(slots, p.scale); } },
      h("option", { value: "" }, "— choose a recipe —"),
      S.recipes.map((s) => h("option", { value: s.id, selected: p.slots[i] === s.id }, `${s.recipe.name} · ${s.recipe.dose_g} g ${ratioText(ratioOf(s.recipe))}`)));
    const rec = S.recipes.find((s) => s.id === p.slots[i]);
    return h("div", { class: "card pad stack" },
      h("div", { class: "row" }, h("div", { class: "slot-letter" }, "ABC"[i]), h("div", { class: "grow" }, h("div", { class: "caption" }, `Slot ${"ABC"[i]}`), h("b", { class: "ellipsis" }, rec ? rec.recipe.name : "Not assigned"))),
      sel,
      setting("Scale", "Weigh the brew on the machine's scale.", sw(p.scale[i], (v) => { const scale = [...p.scale]; scale[i] = v; save(p.slots, scale); }, "Scale")));
  };
  return h("div", {},
    h("div", { class: "page-head" },
      h("div", { class: "grow" }, h("h1", {}, "Dial presets"), h("p", { class: "muted" }, "Program the machine's A · B · C dial, then brew from the machine with no phone.")),
      presetStatus()),
    h("div", { class: "stack" },
      h("div", { class: "grid three" }, slotCard(0), slotCard(1), slotCard(2)),
      h("div", { class: "card pad row wrap" },
        h("div", { class: "grow" },
          h("b", {}, "Write the presets to the machine"),
          h("div", { class: "muted small" }, "All three are stored at once. Close the xBloom phone app first — it holds the only Bluetooth link, and it can overwrite these presets later.")),
        btn("Program the machine", async () => {
          try {
            await api("POST", "/api/presets/push");
            toast("Programming the dial…");
          } catch (ex) { toast(ex.message, "error"); }
        }, "", "bluetooth"))));
}

// ── screensaver ─────────────────────────────────────────────────────────────────────────

async function reloadMedia() {
  S.media = await api("GET", "/api/media");
}

async function patchSection(section, changes) {
  const body = { [section]: { ...S.settings[section], ...changes } };
  S.settings = await api("PUT", "/api/settings", body);
  setAccent(S.settings.appearance.accent);
}

function pageScreensaver() {
  const ss = S.settings.screensaver;
  const upd = async (changes) => {
    try {
      await patchSection("screensaver", changes);
      rerender();
    } catch (ex) { toast(ex.message, "error"); }
  };
  const minutes = (s) => Math.max(1, Math.round(s / 60));
  const sel = (options, value, on) => h("select", { class: "input", style: "width:auto", onchange: (e) => on(Number(e.target.value)) }, options.map(([v, l]) => h("option", { value: v, selected: v === value }, l)));
  const videoInput = h("input", { type: "file", accept: "video/*", class: "hidden", onchange: (e) => e.target.files[0] && videoDialog(e.target.files[0]) });
  const photoInput = h("input", { type: "file", accept: "image/*", multiple: true, class: "hidden", onchange: (e) => e.target.files.length && uploadPhotos([...e.target.files]) });
  const items = S.media.map((m) => {
    const active = m.kind === "video" && ss.video === m.name && ss.mode === "video";
    return h("div", { class: `card media ${active ? "active" : ""}` },
      h("img", { src: `/api/media/${encodeURIComponent(m.name)}/thumb.jpg?v=${m.size}`, alt: "", loading: "lazy" }),
      h("div", { class: "body" },
        h("b", { class: "ellipsis small" }, m.name),
        h("span", { class: "faint small" }, m.kind === "video" ? `${m.width}×${m.height} · ${(m.frames / m.fps).toFixed(0)} s · ${fmtBytes(m.size)}` : `${m.width}×${m.height} photo · ${fmtBytes(m.size)}`),
        h("div", { class: "row" },
          m.kind === "video" && (active ? h("span", { class: "pill accent" }, "In use") : btn("Use", () => upd({ mode: "video", video: m.name }), "small secondary")),
          h("span", { class: "grow" }),
          btn("", async () => {
            if (await confirmDialog(`Delete ${m.name}?`, "It's removed from the screen.")) {
              await api("DELETE", `/api/media/${encodeURIComponent(m.name)}`);
              await reloadMedia();
              S.settings = await api("GET", "/api/settings");
              rerender();
            }
          }, "ghost icon", "trash-2", { "aria-label": `Delete ${m.name}`, title: "Delete" }))));
  });
  return h("div", {},
    h("div", { class: "page-head" },
      h("div", { class: "grow" }, h("h1", {}, "Screensaver"), h("p", { class: "muted" }, `Videos are converted right here in your browser to ${S.info.video.width}×${S.info.video.height} at ${S.info.video.fps} fps — any format your browser can play works.`)),
      h("div", { class: "row wrap" }, btn("Upload photos", () => photoInput.click(), "secondary", "image"), btn("Upload video", () => videoInput.click(), "", "upload"), videoInput, photoInput)),
    h("div", { class: "stack" },
      h("div", { class: "card pad" },
        setting("Show", null, seg([["off", "Nothing"], ["clock", "Clock"], ["video", "Video"], ["slideshow", "Photos"]], ss.mode, (v) => upd({ mode: v }))),
        setting("Start after", "Minutes without a touch.", sel([[1, "1 min"], [2, "2 min"], [5, "5 min"], [10, "10 min"], [15, "15 min"], [30, "30 min"], [60, "1 hour"]], minutes(ss.idle_s), (v) => upd({ idle_s: v * 60 }))),
        setting("Clock on top", "Show the time over videos and photos.", sw(ss.clock_overlay, (v) => upd({ clock_overlay: v }), "Clock on top")),
        ss.mode === "slideshow" && setting("Each photo for", null, sel([[5, "5 s"], [8, "8 s"], [15, "15 s"], [30, "30 s"], [60, "1 min"]], ss.slide_s, (v) => upd({ slide_s: v }))),
        setting("Turn the screen off", "After the screensaver has run this long.", sel([[0, "Never"], [300, "5 min"], [900, "15 min"], [1800, "30 min"], [3600, "1 hour"]], ss.sleep_after_s, (v) => upd({ sleep_after_s: v })))),
      h("div", {},
        h("div", { class: "caption", style: "margin:6px 2px 10px" }, "Library"),
        S.media.length ? h("div", { class: "media-grid" }, items) : h("div", { class: "card empty" }, icon("film"), h("div", {}, "No videos or photos yet.")))));
}

function videoDialog(file) {
  const profile = S.info.video;
  modal((close) => {
    const box = h("div", { class: "stack converter" }, h("h2", {}, "Convert video"), h("div", { class: "row" }, h("div", { class: "spinner" }), h("span", { class: "muted" }, "Reading the video…")));
    let ctrl = null;
    (async () => {
      let probe;
      try {
        probe = await probeVideo(file);
      } catch (ex) {
        box.replaceChildren(h("h2", {}, "Can't read this video"), h("p", { class: "muted" }, `${ex.message}. Try an MP4 (H.264) or WebM file.`), btn("Close", close, "secondary"));
        return;
      }
      const maxLen = Math.min(profile.max_seconds, Math.max(1, probe.duration));
      let start = 0;
      let seconds = Math.min(15, maxLen);
      let smooth = true;
      const preview = probe.video;
      preview.controls = false;
      preview.loop = true;
      preview.play().catch(() => {});
      const label = h("div", { class: "small muted" });
      const est = () => Math.round(seconds * profile.fps * profile.width * profile.height * 0.11 * profile.quality);
      const refresh = () => { label.textContent = `From ${start.toFixed(1)} s, ${seconds.toFixed(1)} s long → ${Math.round(seconds * profile.fps)} frames, about ${fmtBytes(est())}`; };
      const startR = h("input", { type: "range", min: 0, max: Math.max(0, probe.duration - 0.5), step: 0.1, value: 0, oninput: (e) => { start = Number(e.target.value); preview.currentTime = start; seconds = Math.min(seconds, probe.duration - start); refresh(); } });
      const lenR = h("input", { type: "range", min: 1, max: maxLen, step: 0.5, value: seconds, oninput: (e) => { seconds = Number(e.target.value); refresh(); } });
      const name = h("input", { class: "input", value: file.name.replace(/\.[^.]+$/, ""), maxlength: 48 });
      const bar = h("div", { class: "bar" }, h("div"));
      const status = h("div", { class: "small muted" });
      const go = btn("Convert & upload", async () => {
        go.disabled = true;
        ctrl = new AbortController();
        try {
          preview.pause();
          const blob = await convertVideo(preview, profile, { start, seconds, smoothLoop: smooth, signal: ctrl.signal, onProgress: (i, n) => {
            bar.firstChild.style.width = `${(i / n) * 70}%`;
            status.textContent = `Converting frame ${i} of ${n}…`;
          } });
          status.textContent = `Uploading ${fmtBytes(blob.size)}…`;
          const info = await upload(`/api/media?name=${encodeURIComponent(`${name.value || "video"}.xbv`)}`, blob, (f) => { bar.firstChild.style.width = `${70 + f * 30}%`; });
          await reloadMedia();
          await patchSection("screensaver", { mode: "video", video: info.name });
          toast("Video uploaded and set as the screensaver", "success");
          close();
          rerender();
        } catch (ex) {
          if (ex.name !== "AbortError") toast(ex.message, "error");
          go.disabled = false;
          status.textContent = "";
        }
      }, "", "upload");
      refresh();
      box.replaceChildren(
        h("h2", {}, "Convert video"),
        preview,
        h("label", { class: "field" }, "Start", startR),
        h("label", { class: "field" }, `Length (up to ${maxLen.toFixed(0)} s)`, lenR),
        label,
        h("label", { class: "field" }, "Name", name),
        setting("Seamless loop", "Fade the end into the start.", sw(true, (v) => (smooth = v), "Seamless loop")),
        bar, status,
        h("div", { class: "row", style: "justify-content:flex-end" }, btn("Cancel", () => { ctrl?.abort(); close(); }, "secondary"), go));
    })();
    return box;
  }, { wide: true });
}

async function uploadPhotos(files) {
  const profile = S.info.video;
  let done = 0;
  for (const f of files) {
    try {
      const jpg = await convertPhoto(f, profile);
      await upload(`/api/media?name=${encodeURIComponent(f.name.replace(/\.[^.]+$/, "") + ".jpg")}`, jpg);
      done++;
    } catch (ex) {
      toast(`${f.name}: ${ex.message}`, "error");
    }
  }
  await reloadMedia();
  if (done) {
    toast(`${done} photo${done === 1 ? "" : "s"} uploaded`, "success");
    if (S.settings.screensaver.mode !== "slideshow" && S.settings.screensaver.mode !== "video") await patchSection("screensaver", { mode: "slideshow" });
  }
  rerender();
}

// ── history ─────────────────────────────────────────────────────────────────────────────

function chart(curve, target) {
  const NS = "http://www.w3.org/2000/svg";
  const W = 600, H = 220;
  const svg = document.createElementNS(NS, "svg");
  svg.setAttribute("viewBox", `0 0 ${W} ${H}`);
  svg.setAttribute("class", "chart");
  svg.setAttribute("preserveAspectRatio", "none");
  const tMax = Math.max(30, ...curve.map((p) => p[0]));
  const gMax = Math.max(50, target * 1.12, ...curve.map((p) => Math.max(p[1], p[2])) ) * 1.04;
  for (let i = 1; i < 4; i++) {
    const l = document.createElementNS(NS, "line");
    Object.entries({ x1: 0, x2: W, y1: (H * i) / 4, y2: (H * i) / 4, class: "grid", "stroke-width": 1 }).forEach(([k, v]) => l.setAttribute(k, v));
    svg.append(l);
  }
  const path = (idx, cls) => {
    const p = document.createElementNS(NS, "polyline");
    p.setAttribute("points", curve.map((c) => `${((c[0] / tMax) * W).toFixed(1)},${(H - (c[idx] / gMax) * H).toFixed(1)}`).join(" "));
    p.setAttribute("fill", "none");
    p.setAttribute("class", cls);
    p.setAttribute("stroke-width", 2.5);
    p.setAttribute("vector-effect", "non-scaling-stroke");
    p.setAttribute("stroke-linejoin", "round");
    svg.append(p);
  };
  path(2, "coffee");
  path(1, "water");
  return svg;
}

function pageHistory() {
  const rows = S.history.map((e) =>
    h("div", { class: "recipe-row", onclick: () => historyDialog(e.id) },
      h("div", { class: "chipdot", style: `background:${e.outcome === "completed" ? "var(--success)" : "var(--warning)"}` }, icon(e.outcome === "completed" ? "coffee" : "triangle-alert")),
      h("div", { class: "grow" },
        h("b", { class: "ellipsis" }, e.recipe),
        h("div", { class: "meta" }, h("span", {}, fmtDate(e.ts)), h("span", {}, `${e.dose_g} g · ${ratioText(e.ratio)}`), h("span", {}, `${Math.round(e.water_g)} g water`), h("span", {}, fmtTime(e.duration_s)))),
      h("span", { class: "faint" }, icon("chevron-right"))));
  return h("div", {},
    h("div", { class: "page-head" },
      h("div", { class: "grow" }, h("h1", {}, "History"), h("p", { class: "muted" }, `${S.history.length} brew${S.history.length === 1 ? "" : "s"} with their water and coffee curves.`)),
      h("div", { class: "row wrap" },
        h("a", { class: "btn secondary", href: "/api/history/export.csv", download: "xbloom-history.csv" }, icon("download"), "CSV"),
        S.history.length > 0 && btn("Clear", async () => {
          if (await confirmDialog("Clear the history?", "All brews and their curves are deleted.", "Clear history")) {
            await api("DELETE", "/api/history");
            S.history = [];
            rerender();
          }
        }, "ghost", "trash-2"))),
    h("div", { class: "card", style: "overflow:hidden" }, rows.length ? rows : h("div", { class: "empty" }, icon("history"), h("div", {}, "No brews yet."))));
}

async function historyDialog(id) {
  let e;
  try { e = await api("GET", `/api/history/${id}`); } catch (ex) { return toast(ex.message, "error"); }
  modal((close) => h("div", { class: "stack" },
    h("div", { class: "row" }, h("div", { class: "grow" }, h("h2", {}, e.recipe), h("div", { class: "muted small" }, fmtDate(e.ts))), btn("", close, "ghost icon", "x", { "aria-label": "Close" })),
    h("div", { class: "row wrap small" },
      h("span", { class: "pill" }, `${e.dose_g} g · ${ratioText(e.ratio)}`),
      h("span", { class: "pill" }, e.grind === 0 ? "pre-ground" : `grind ${e.grind}`),
      h("span", { class: "pill" }, `${Math.round(e.water_g)} g water`),
      h("span", { class: "pill" }, `${Math.round(e.coffee_g)} g in cup`),
      h("span", { class: "pill" }, fmtTime(e.duration_s)),
      h("span", { class: "pill" }, e.remote_start ? "started from the screen" : "started on the machine")),
    h("div", { class: "legend" }, h("span", {}, h("i", { style: "background:var(--water)" }), "Water"), h("span", {}, h("i", { style: "background:var(--accent)" }), "In cup")),
    e.curve.length ? chart(e.curve, e.water_g) : h("p", { class: "muted" }, "No curve was recorded."),
    h("div", { class: "row", style: "justify-content:flex-end" },
      btn("Delete", async () => { await api("DELETE", `/api/history/${id}`); S.history = await api("GET", "/api/history"); close(); rerender(); }, "ghost", "trash-2"))), { wide: true });
}

// ── settings ────────────────────────────────────────────────────────────────────────────

function pinDialog(kind) {
  modal((close) => {
    const pinInput = (label, auto) => h("label", { class: "field" }, label, h("input", { class: "input", type: "password", inputmode: "numeric", maxlength: 8, autocomplete: auto }));
    const admin = pinInput(kind === "admin" ? "Current admin PIN" : "Admin PIN", "current-password");
    const n1 = pinInput(kind === "admin" ? "New admin PIN" : "New lock PIN", "new-password");
    const n2 = pinInput("Confirm the new PIN", "new-password");
    const err = h("div", { class: "small", style: "color:var(--danger);min-height:18px" });
    const v = (l) => l.querySelector("input").value;
    const go = btn("Save PIN", async () => {
      err.textContent = "";
      if (!/^\d{4,8}$/.test(v(n1))) return (err.textContent = "Use 4 to 8 digits.");
      if (v(n1) !== v(n2)) return (err.textContent = "The PINs don't match.");
      try {
        if (kind === "admin") await api("POST", "/api/pin/admin", { current: v(admin), new: v(n1) });
        else await api("POST", "/api/pin/user", { admin_pin: v(admin), new: v(n1) });
        S.settings = await api("GET", "/api/settings");
        toast(kind === "admin" ? "Admin PIN changed" : "Lock PIN set", "success");
        close();
        rerender();
      } catch (ex) { err.textContent = ex.message; }
    }, "", "check");
    return h("div", { class: "stack" }, h("h2", {}, kind === "admin" ? "Change the admin PIN" : "Set the lock PIN"),
      h("p", { class: "muted small", style: "margin:0" }, kind === "admin" ? "Used for the screen's settings and this panel." : "Unlocks the screen and brewing when the lock is on. The admin PIN works too."),
      admin, n1, n2, err, h("div", { class: "row", style: "justify-content:flex-end" }, btn("Cancel", close, "secondary"), go));
  });
}

// POSIX TZ rules (what the ESP32's C library understands) for common zones, keyed by the
// IANA name a browser reports so the panel can suggest the right one.
const ZONES = [
  ["UTC0", "UTC", ["UTC", "Etc/UTC"]],
  ["GMT0BST,M3.5.0/1,M10.5.0", "London, Dublin, Lisbon", ["Europe/London", "Europe/Dublin", "Europe/Lisbon"]],
  ["CET-1CEST,M3.5.0,M10.5.0/3", "Central Europe — Paris, Berlin, Madrid, Rome", ["Europe/Paris", "Europe/Berlin", "Europe/Madrid", "Europe/Rome", "Europe/Amsterdam", "Europe/Brussels", "Europe/Vienna", "Europe/Zurich", "Europe/Stockholm", "Europe/Oslo", "Europe/Copenhagen", "Europe/Prague", "Europe/Warsaw"]],
  ["EET-2EEST,M3.5.0/3,M10.5.0/4", "Eastern Europe — Athens, Helsinki, Kyiv", ["Europe/Athens", "Europe/Helsinki", "Europe/Kyiv", "Europe/Kiev", "Europe/Bucharest", "Europe/Sofia"]],
  ["EET-2", "Cairo", ["Africa/Cairo"]],
  ["<+03>-3", "UTC+3 — Istanbul, Riyadh, Moscow, Nairobi", ["Europe/Istanbul", "Asia/Riyadh", "Europe/Moscow", "Asia/Qatar", "Asia/Kuwait", "Asia/Baghdad", "Africa/Nairobi"]],
  ["<+04>-4", "UTC+4 — Dubai, Abu Dhabi, Muscat", ["Asia/Dubai", "Asia/Muscat", "Asia/Baku", "Asia/Tbilisi"]],
  ["<+05>-5", "UTC+5 — Karachi, Tashkent", ["Asia/Karachi", "Asia/Tashkent"]],
  ["IST-5:30", "India", ["Asia/Kolkata", "Asia/Calcutta"]],
  ["<+07>-7", "UTC+7 — Bangkok, Jakarta, Ho Chi Minh City", ["Asia/Bangkok", "Asia/Jakarta", "Asia/Ho_Chi_Minh"]],
  ["<+08>-8", "UTC+8 — Singapore, Hong Kong, Beijing, Perth", ["Asia/Singapore", "Asia/Hong_Kong", "Asia/Shanghai", "Asia/Taipei", "Asia/Manila", "Australia/Perth", "Asia/Kuala_Lumpur"]],
  ["<+09>-9", "UTC+9 — Tokyo, Seoul", ["Asia/Tokyo", "Asia/Seoul"]],
  ["AEST-10AEDT,M10.1.0,M4.1.0/3", "Sydney, Melbourne, Canberra", ["Australia/Sydney", "Australia/Melbourne", "Australia/Canberra", "Australia/Hobart"]],
  ["AEST-10", "Brisbane", ["Australia/Brisbane"]],
  ["NZST-12NZDT,M9.5.0,M4.1.0/3", "New Zealand", ["Pacific/Auckland"]],
  ["<-03>3", "UTC−3 — São Paulo, Buenos Aires", ["America/Sao_Paulo", "America/Argentina/Buenos_Aires", "America/Montevideo"]],
  ["EST5EDT,M3.2.0,M11.1.0", "US & Canada Eastern", ["America/New_York", "America/Toronto", "America/Detroit"]],
  ["CST6CDT,M3.2.0,M11.1.0", "US & Canada Central", ["America/Chicago", "America/Winnipeg"]],
  ["CST6", "Mexico City", ["America/Mexico_City"]],
  ["MST7MDT,M3.2.0,M11.1.0", "US & Canada Mountain", ["America/Denver", "America/Edmonton"]],
  ["MST7", "Arizona", ["America/Phoenix"]],
  ["PST8PDT,M3.2.0,M11.1.0", "US & Canada Pacific", ["America/Los_Angeles", "America/Vancouver"]],
  ["AKST9AKDT,M3.2.0,M11.1.0", "Alaska", ["America/Anchorage"]],
  ["HST10", "Hawaii", ["Pacific/Honolulu"]],
];

function zonePicker(current, onpick) {
  const browser = (() => { try { return Intl.DateTimeFormat().resolvedOptions().timeZone; } catch { return ""; } })();
  const suggested = ZONES.find((z) => z[2].includes(browser));
  const known = ZONES.some((z) => z[0] === current);
  const custom = h("input", { class: "input", value: known ? "" : current, placeholder: "POSIX TZ, e.g. CET-1CEST,M3.5.0,M10.5.0/3", style: `max-width:320px;${known ? "display:none" : ""}`,
    onchange: (e) => e.target.value.trim() && onpick(e.target.value.trim()) });
  const select = h("select", { class: "input", style: "width:auto;max-width:320px", onchange: (e) => {
    if (e.target.value === "custom") { custom.style.display = ""; custom.focus(); } else onpick(e.target.value);
  } },
  ZONES.map(([tz, label]) => h("option", { value: tz, selected: tz === current }, label)),
  h("option", { value: "custom", selected: !known }, "Other (POSIX rule)…"));
  return h("div", { class: "stack", style: "gap:8px;align-items:flex-end" }, select, custom,
    suggested && suggested[0] !== current && btn(`Use this browser’s zone (${browser})`, () => onpick(suggested[0]), "ghost small"));
}

function pageSettings() {
  const s = S.settings;
  const run = async (fn) => {
    try {
      await fn();
      rerender();
    } catch (ex) { toast(ex.message, "error"); }
  };
  const patch = (body) => run(async () => { S.settings = await api("PUT", "/api/settings", body); setAccent(S.settings.appearance.accent); });
  const sel = (options, value, on) => h("select", { class: "input", style: "width:auto", onchange: (e) => on(Number(e.target.value)) }, options.map(([v, l]) => h("option", { value: v, selected: v === value }, l)));
  const name = h("input", { class: "input", value: s.device_name, maxlength: 40, style: "max-width:260px", onchange: (e) => patch({ device_name: e.target.value }) });
  const pref = localStorage.getItem("xb-theme") || "system";
  const esp = S.info.platform === "esp32s3";
  const ssid = h("input", { class: "input", value: s.wifi.ssid, placeholder: "Network name", autocomplete: "off" });
  const pw = h("input", { class: "input", type: "password", placeholder: s.wifi.has_password ? "•••••••• (unchanged)" : "Password", autocomplete: "new-password" });
  const restoreInput = h("input", { type: "file", accept: ".json,application/json", class: "hidden", onchange: async (e) => {
    const f = e.target.files[0];
    if (!f) return;
    if (!(await confirmDialog("Restore this backup?", "Recipes, presets and settings are replaced (PINs and Wi-Fi are kept).", "Restore", false))) return;
    run(async () => {
      await api("POST", "/api/restore", await f.text());
      await loadAll();
      toast("Backup restored", "success");
    });
  } });
  return h("div", {},
    h("div", { class: "page-head" }, h("div", { class: "grow" }, h("h1", {}, "Settings"), h("p", { class: "muted" }, "Changes apply to the screen immediately."))),
    h("div", { class: "stack" },
      h("div", { class: "card pad" },
        h("h3", {}, "Screen"),
        setting("Name", "Shown here and on the screen.", name),
        setting("Theme", null, seg([["light", "Light"], ["dark", "Dark"], ["auto", "Auto"]], s.appearance.theme, (v) => run(() => patchSection("appearance", { theme: v })))),
        setting("Accent colour", null, h("div", { class: "swatches" }, ["#c8743a", "#b5523b", "#8a6a3d", "#3f8f6b", "#3a7cb8", "#7a5cc0"].map((c) =>
          h("button", { type: "button", class: `swatch ${s.appearance.accent === c ? "on" : ""}`, style: `background:${c}`, "aria-label": c, onclick: () => run(() => patchSection("appearance", { accent: c })) })))),
        setting("Brightness", `${s.appearance.brightness}%`, h("input", { type: "range", min: 10, max: 100, value: s.appearance.brightness, style: "max-width:220px", onchange: (e) => run(() => patchSection("appearance", { brightness: Number(e.target.value) })) })),
        setting("Dimmed brightness", "While the screensaver runs.", sel([[0, "Off"], [10, "10%"], [25, "25%"], [40, "40%"], [60, "60%"]], s.appearance.dim_brightness, (v) => run(() => patchSection("appearance", { dim_brightness: v })))),
        setting("24-hour clock", null, sw(s.appearance.clock_24h, (v) => run(() => patchSection("appearance", { clock_24h: v })), "24-hour clock"))),
      h("div", { class: "card pad" },
        h("h3", {}, "Lock & PINs"),
        setting("Lock", s.security.lock_mode === "off" ? "Anyone can brew; settings always need the admin PIN." : s.security.lock_mode === "brew" ? "Browsing is free; starting a brew asks for a PIN." : "The screen locks when idle; a PIN unlocks it.",
          seg([["off", "Off"], ["brew", "Brewing"], ["screen", "Whole screen"]], s.security.lock_mode, (v) => patch({ security: { lock_mode: v } }))),
        setting("Re-lock after", "Idle time before the screen locks again.", sel([[60, "1 min"], [300, "5 min"], [600, "10 min"], [1800, "30 min"], [3600, "1 hour"]], s.security.auto_lock_s, (v) => patch({ security: { auto_lock_s: v } }))),
        setting("Lock PIN", s.security.has_user_pin ? "Set — a separate PIN for unlocking and brewing." : "Not set — the admin PIN unlocks.",
          h("div", { class: "row" },
            s.security.has_user_pin && btn("Remove", () => {
              modal((close) => {
                const a = h("input", { class: "input", type: "password", inputmode: "numeric", maxlength: 8, placeholder: "Admin PIN" });
                return h("div", { class: "stack" }, h("h2", {}, "Remove the lock PIN"), a,
                  h("div", { class: "row", style: "justify-content:flex-end" }, btn("Cancel", close, "secondary"),
                    btn("Remove", async () => { try { await api("POST", "/api/pin/user", { admin_pin: a.value, new: null }); S.settings = await api("GET", "/api/settings"); close(); rerender(); toast("Lock PIN removed"); } catch (ex) { toast(ex.message, "error"); } }, "danger")));
              });
            }, "ghost small"),
            btn(s.security.has_user_pin ? "Change" : "Set PIN", () => pinDialog("user"), "secondary small"))),
        setting("Admin PIN", "For the screen's settings and this panel.", btn("Change", () => pinDialog("admin"), "secondary small")),
        setting("Remote start", "Show “Hold to brew” on the screen. Off: the screen only loads recipes and you press start on the machine.",
          sw(s.security.allow_remote_start, (v) => patch({ security: { allow_remote_start: v } }), "Remote start")),
        setting("Hold to brew for", "How long the button must be held — the safety gesture.", sel([[800, "0.8 s"], [1200, "1.2 s"], [1800, "1.8 s"], [2500, "2.5 s"]], s.security.hold_to_brew_ms, (v) => patch({ security: { hold_to_brew_ms: v } })))),
      h("div", { class: "card pad" },
        h("h3", {}, "Machine"),
        setting("Paired machine", s.machine.address ? `${s.machine.name || "xBloom"} · ${s.machine.address}` : "None — pair one from Home or the screen.",
          s.machine.address && btn("Forget", async () => { if (await confirmDialog("Forget the machine?", "The screen disconnects and you'll need to pair again.", "Forget")) run(async () => { await api("POST", "/api/machine/forget"); S.settings = await api("GET", "/api/settings"); }); }, "ghost small")),
        setting("Connect automatically", "Hold the link open for instant brews; reconnect when it drops.", sw(s.machine.auto_connect, (v) => patch({ machine: { auto_connect: v } }), "Connect automatically")),
        setting("Demo mode", "Drive a built-in simulated machine — try everything without coffee.", sw(s.machine.simulator, (v) => patch({ machine: { simulator: v } }), "Demo mode"))),
      esp && h("div", { class: "card pad stack" },
        h("h3", {}, "Wi-Fi & clock"),
        h("p", { class: "muted small" }, "Without a network the screen hosts its own Wi-Fi (shown under Settings → Admin panel on the screen)."),
        h("div", { class: "grid two" }, h("label", { class: "field" }, "Network", ssid), h("label", { class: "field" }, "Password", pw)),
        h("div", {}, btn("Save Wi-Fi", () => patch({ wifi: { ssid: ssid.value, password: pw.value || undefined } }), "secondary small", "wifi")),
        setting("Time zone", "The clock is set from the internet once the screen is online.", zonePicker(s.timezone, (tz) => patch({ timezone: tz })))),
      h("div", { class: "card pad" },
        h("h3", {}, "Backup & device"),
        setting("Backup", "Recipes, presets and settings (no PINs, no Wi-Fi password).",
          h("div", { class: "row" }, h("a", { class: "btn secondary small", href: "/api/backup", download: "xbloom-screen-backup.json" }, icon("download"), "Download"), btn("Restore", () => restoreInput.click(), "ghost small", "upload"), restoreInput)),
        setting("Lock the screen now", null, btn("Lock", async () => { await api("POST", "/api/device/lock"); toast("Screen locked"); }, "secondary small", "lock")),
        setting("Restart the screen", null, btn("Restart", async () => { if (await confirmDialog("Restart the screen?", "It's back in a few seconds.", "Restart", false)) { await api("POST", "/api/device/restart"); toast("Restarting…"); } }, "outline small", "power")),
        setting("This browser", "Light or dark for this admin panel.", seg([["system", "System"], ["light", "Light"], ["dark", "Dark"]], pref, (v) => { localStorage.setItem("xb-theme", v); applyTheme(); rerender(); }))),
      h("p", { class: "faint small", style: "text-align:center" }, `${S.info.device_name} · v${S.info.version} · ${S.info.screen.width}×${S.info.screen.height}`, S.info.storage ? ` · ${fmtBytes(S.info.storage.free)} free` : "", " · unofficial, not affiliated with xBloom")));
}

boot();
