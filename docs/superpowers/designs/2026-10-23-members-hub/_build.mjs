// Builds the Members hub comps (static HTML) from one set of realistic data.
// Run: node docs/superpowers/designs/2026-10-23-members-hub/_build.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const OUT = dirname(fileURLToPath(import.meta.url));

// ---------------------------------------------------------------- icons (lucide, 24 grid)
const ICONS = {
  users: '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>',
  box: '<path d="M21 8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16Z"/><path d="m3.3 7 8.7 5 8.7-5"/><path d="M12 22V12"/>',
  table: '<path d="M9 3H5a2 2 0 0 0-2 2v4m6-6h10a2 2 0 0 1 2 2v4M9 3v18m0 0h10a2 2 0 0 0 2-2V9M9 21H5a2 2 0 0 1-2-2V9m0 0h18"/>',
  book: '<path d="M12 7v14"/><path d="M16 12h2"/><path d="M16 8h2"/><path d="M3 18a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h5a4 4 0 0 1 4 4 4 4 0 0 1 4-4h5a1 1 0 0 1 1 1v13a1 1 0 0 1-1 1h-6a3 3 0 0 0-3 3 3 3 0 0 0-3-3z"/><path d="M6 12h2"/><path d="M6 8h2"/>',
  shield: '<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/><path d="m9 12 2 2 4-4"/>',
  more: '<circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/><circle cx="5" cy="12" r="1"/>',
  down: '<path d="m6 9 6 6 6-6"/>',
  right: '<path d="m9 18 6-6-6-6"/>',
  lock: '<rect width="18" height="11" x="3" y="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>',
  search: '<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/>',
  grid: '<rect width="7" height="7" x="3" y="3" rx="1"/><rect width="7" height="7" x="14" y="3" rx="1"/><rect width="7" height="7" x="14" y="14" rx="1"/><rect width="7" height="7" x="3" y="14" rx="1"/>',
  list: '<path d="M3 12h.01"/><path d="M3 18h.01"/><path d="M3 6h.01"/><path d="M8 12h13"/><path d="M8 18h13"/><path d="M8 6h13"/>',
  plus: '<path d="M5 12h14"/><path d="M12 5v14"/>',
  x: '<path d="M18 6 6 18"/><path d="m6 6 12 12"/>',
  copy: '<rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>',
  key: '<path d="M2.586 17.414A2 2 0 0 0 2 18.828V21a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h1a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h.172a2 2 0 0 0 1.414-.586l.814-.814a6.5 6.5 0 1 0-4-4z"/><circle cx="16.5" cy="7.5" r=".5"/>',
  back: '<path d="m12 19-7-7 7-7"/><path d="M19 12H5"/>',
  user: '<circle cx="12" cy="12" r="10"/><circle cx="12" cy="10" r="3"/><path d="M7 20.662V19a2 2 0 0 1 2-2h6a2 2 0 0 1 2 2v1.662"/>',
  pclose: '<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M15 3v18"/><path d="m8 9 3 3-3 3"/>',
  sidebar: '<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M9 3v18"/>',
  menu: '<path d="M4 12h16"/><path d="M4 6h16"/><path d="M4 18h16"/>',
  renew: '<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>',
  cols: '<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M9 3v18"/><path d="M15 3v18"/>',
  ext: '<path d="M15 3h6v6"/><path d="M10 14 21 3"/><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/>',
};
const sprite = `<svg width="0" height="0" style="position:absolute" aria-hidden="true"><defs>${Object.entries(ICONS)
  .map(([k, v]) => `<symbol id="i-${k}" viewBox="0 0 24 24">${v}</symbol>`)
  .join("")}</defs></svg>`;
const I = (n, c = "") => `<svg class="i ${c}" aria-hidden="true"><use href="#i-${n}"/></svg>`;

// ---------------------------------------------------------------- small parts
const cb = (state = "", label = "") => `<span class="cb ${state}" role="checkbox" aria-checked="${state.includes("on") ? "true" : state.includes("mixed") ? "mixed" : "false"}"${label ? ` aria-label="${label}"` : ""}></span>`;
const LV = { tenant_admin: "Tenant admin", admin: "Project admin", developer: "Developer", viewer: "Viewer", other: "Other projects", none: "No access yet" };
const lv = (l) => (l === "tenant_admin" ? `<span class="tag chalk">${I("shield", "s12")}Tenant admin</span>` : `<span class="tag">${LV[l]}</span>`);
// A role name as Studio renders it: control, bidi and non-ASCII characters shown escaped (MH-38d).
const rn = (name) =>
  [...name]
    .map((ch) => {
      const c = ch.codePointAt(0);
      if (c < 0x20 || c === 0x7f || c > 0x7e) return `<span class="esc" title="U+${c.toString(16).toUpperCase().padStart(4, "0")}">\\u${c.toString(16).padStart(4, "0")}</span>`;
      return ch.replace(/&/g, "&amp;").replace(/</g, "&lt;");
    })
    .join("");
const initials = (n) => n.split(/[ .@]/).filter(Boolean).slice(0, 2).map((s) => s[0].toUpperCase()).join("");
const select = (label, value, cls = "") => `<label class="select-l ${cls}"><span>${label}</span><span class="field select"><span class="v">${value}</span>${I("down", "s16 dim")}</span></label>`;

function page({ file, title, crumbs, body, overlay = "", wide = false, layout = null, note }) {
  const html = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${title}</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Atkinson+Hyperlegible+Next:wght@400;500;600&family=JetBrains+Mono:wght@400;500&display=swap" rel="stylesheet">
<link rel="stylesheet" href="comp.css">
</head>
<body>
${sprite}
<a class="sr" href="#content">Skip to content</a>
<div class="shell">
  <aside class="rail" aria-label="Studio"><div class="rail-in">
    <div class="home"><span class="mark" aria-label="Chelabase"></span></div>
    <nav>
      <a class="item" href="#" title="Projects">${I("box")}</a>
      <a class="item" href="#" title="Table editor">${I("table")}</a>
      <a class="item" href="#" title="API reference">${I("book")}</a>
      <a class="item on" href="#" aria-current="page" title="Members, 1 request waiting">${I("users")}</a>
    </nav>
    <div class="foot">${I("sidebar")}</div>
  </div></aside>
  <div class="col">
    <header class="topbar">
      <button class="btn icon ghost menu-btn" aria-label="Menu">${I("menu")}</button>
      <div class="crumbs">${crumbs}</div>
      <div class="acct">${I("user")}<span>${VIEWER}</span></div>
    </header>
    <main class="main" id="content">${layout ? layout : `<div class="page${wide ? " wide" : ""}">${body}</div>`}</main>
  </div>
</div>
${overlay}
</body>
</html>
`;
  writeFileSync(join(OUT, file), html);
  return { file, title, note };
}
let VIEWER = "Mara Okafor";

// ---------------------------------------------------------------- data
const P = { bill: "Billing app", ops: "Ops console", wh: "Data warehouse" };
const people = [
  { id: "mara", n: "Mara Okafor", e: "mara@northwind.dev", lvl: "tenant_admin", proj: 3, joined: "4 Mar 2026", you: true },
  { id: "jonas", n: "Jonas Whitfield", e: "jonas@northwind.dev", lvl: "tenant_admin", proj: 3, joined: "4 Mar 2026" },
  { id: "priya", n: "Priya Raman", e: "priya.raman@northwind.dev", lvl: "admin", proj: 2, joined: "11 Mar 2026", on: { bill: "admin", ops: "developer" } },
  { id: "tomas", n: "Tomás Ibarra", e: "tomas@northwind.dev", lvl: "admin", proj: 1, joined: "2 Apr 2026", on: { wh: "admin" } },
  { id: "lena", n: "Lena Kowalski", e: "lena.k@northwind.dev", lvl: "developer", proj: 2, joined: "18 Apr 2026", on: { bill: "developer", wh: "viewer" } },
  { id: "sofia", n: "Sofia Marchetti", e: "sofia@northwind.dev", lvl: "developer", proj: 2, joined: "9 May 2026", on: { bill: "developer", wh: "developer" } },
  { id: "aiden", n: "Aiden Brooks", e: "aiden@northwind.dev", lvl: "developer", proj: 1, joined: "21 May 2026", on: { ops: "developer" } },
  { id: "kenji", n: "Kenji Watanabe", e: "kenji.w@northwind.dev", lvl: "developer", proj: 1, joined: "3 Jun 2026", on: { wh: "developer" } },
  { id: "hannah", n: "Hannah Lindqvist", e: "hannah@northwind.dev", lvl: "viewer", proj: 1, joined: "30 Jun 2026", on: { bill: "viewer" } },
  { id: "omar", n: "Omar Haddad", e: "omar.haddad@northwind.dev", lvl: "viewer", proj: 2, joined: "14 Jul 2026", on: { ops: "viewer", wh: "viewer" } },
  { id: "grace", n: "Grace Liu", e: "grace.liu@northwind.dev", lvl: "none", proj: 0, joined: "19 Oct 2026" },
  { id: "rui", n: "rui.costa", e: "rui.costa@northwind.dev", lvl: "invited", status: "Invited · expires in 2 days", proj: 1, on: { bill: "developer" } },
  { id: "nadia", n: "nadia.petrova", e: "nadia.petrova@northwind.dev", lvl: "invited", status: '<span class="coral">Invite expired</span>', proj: 1, on: { ops: "viewer" } },
  { id: "felix", n: "Felix Bauer", e: "felix.bauer@northwind.dev", lvl: "suspended", status: '<span class="coral">Suspended</span>', proj: 2, joined: "6 May 2026", on: { bill: "developer", ops: "viewer" } },
];
const GROUPS = [
  ["tenant_admin", "Tenant admins"],
  ["admin", "Project admins"],
  ["developer", "Developers"],
  ["viewer", "Viewers"],
  ["other", "Other projects"],
  ["none", "No access yet"],
  ["invited", "Invited"],
  ["suspended", "Suspended"],
];
const GROUP_HINT = {
  tenant_admin: "Admin on every project",
  other: "Access only on projects you can't see",
  none: "Signed in, no project yet",
  invited: "Access counts once they accept",
  suspended: "Can't sign in; access kept for reinstating",
};

// ---------------------------------------------------------------- roster
function card(p, { admin }) {
  const level = p.lvl === "invited" || p.lvl === "suspended" ? p.prevLvl : p.lvl;
  const name = p.lvl === "invited" ? p.e : p.n;
  return `<a class="card${p.hover ? " hover" : ""}" href="04-member-admin-view.html" aria-label="${name}">
  <div class="who"><span class="av${p.lvl === "invited" ? " ghost" : ""}">${initials(p.n)}</span><div>
    <div class="nm">${p.lvl === "invited" ? `<span class="mono" style="font-weight:500">${p.e}</span>` : `${p.n}${p.you ? ' <span class="dim" style="font-weight:400">(you)</span>' : ""}`}</div>
    <div class="em">${p.lvl === "invited" ? "Not signed in yet" : p.e}</div></div></div>
  <div class="meta">${level ? lv(level) : ""}${p.proj ? `<span>${p.proj} project${p.proj > 1 ? "s" : ""}</span>` : '<span>No projects</span>'}${p.status ? `<span>· ${p.status}</span>` : ""}</div>
  ${admin && !p.you ? `<span class="kebab">${I("more", "s16")}</span>` : ""}
</a>`;
}
function row(p, { admin, levelOverride }) {
  const level = levelOverride ?? (p.lvl === "invited" ? null : p.lvl === "suspended" ? "developer" : p.lvl);
  return `<tr>
  <td><div class="person"><span class="av${p.lvl === "invited" ? " ghost" : ""}">${initials(p.n)}</span><div>
    <a href="04-member-admin-view.html">${p.lvl === "invited" ? `<span class="mono" style="font-weight:500">${p.e}</span>` : p.n}</a>${p.you ? ' <span class="dim">(you)</span>' : ""}
    <div class="em">${p.lvl === "invited" ? "Not signed in yet" : p.e}</div></div></div></td>
  <td class="c-level">${level ? lv(level) : '<span class="dim">—</span>'}</td>
  <td class="c-proj num">${p.proj || "—"}</td>
  <td class="c-status status">${p.status ?? (p.lvl === "none" ? '<span class="dim">Active</span>' : "Active")}</td>
  <td class="c-joined num">${p.joined ?? "—"}</td>
  <td class="c-menu" style="width:44px">${admin && !p.you ? `<button class="btn icon sm ghost" aria-label="More actions for ${p.n}">${I("more", "s16")}</button>` : ""}</td>
</tr>`;
}
const membersHead = (tabsOn = "people", admin = true) => `
<h1>Members</h1>
<p class="lede">People in Northwind Labs and what they can reach.${admin ? ' <button class="btn sm ghost" style="margin-left:4px">Rename</button>' : ""}</p>
<div class="tabs" role="tablist" aria-label="Members sections">
  <span role="tab" class="${tabsOn === "people" ? "on" : ""}">People</span>
  <span role="tab" class="${tabsOn === "roles" ? "on" : ""}">Roles</span>
  ${admin ? `<span role="tab">Activity</span><span role="tab">Instance</span>` : ""}
</div>`;
const viewToggle = (v) => `<div class="seg icons" role="radiogroup" aria-label="View"><span role="radio" aria-checked="${v === "cards"}" class="${v === "cards" ? "on" : ""}" title="Cards">${I("grid", "s16")}</span><span role="radio" aria-checked="${v === "list"}" class="${v === "list" ? "on" : ""}" title="List">${I("list", "s16")}</span></div>`;
const toolbar = ({ count, view, admin, project = "All projects" }) => `
<div class="toolbar">
  <span class="count">${count}</span>
  <span class="field">${I("search", "s16 dim")}<span class="ph">Filter by name or email</span></span>
  <span class="field select" style="width:180px"><span class="v">${project}</span>${I("down", "s16 dim")}</span>
  ${viewToggle(view)}
  ${admin ? `<button class="btn primary">${I("plus", "s16")}Invite people</button>` : ""}
</div>
${admin ? "" : '<p class="note dim" style="margin-top:8px">Tenant admins invite and remove people. Levels count only the projects you can see.</p>'}`;

const requests = `
<section class="requests" aria-labelledby="req-h">
  <div class="rh"><h2 id="req-h">Requests <span class="dim" style="font-weight:400">1</span></h2>
  <p class="note dim" style="margin-top:2px">These people signed up and are waiting for a tenant admin. Approve them with the projects they need, or decline.</p></div>
  <div class="row"><span class="av">SO</span><div style="flex:1;min-width:0"><b style="font-weight:600">Sam Ortega</b><div class="dim note">sam@ortega.io · signed up 3 hours ago</div></div>
  <button class="btn outline sm">Approve…</button><button class="btn sm danger">Decline</button></div>
</section>`;

function rosterCards() {
  VIEWER = "Mara Okafor";
  const body = `${membersHead()}${requests}${toolbar({ count: "14 people", view: "cards", admin: true })}
${GROUPS.filter(([g]) => g !== "other")
  .map(([g, label]) => {
    const list = people.filter((p) => p.lvl === g).map((p) => ({ ...p, prevLvl: g === "suspended" ? "developer" : undefined, hover: p.id === "sofia" }));
    return `<section class="group"><div class="group-h"><h2>${label}</h2><span class="n">${list.length}</span>${GROUP_HINT[g] ? `<span class="hint">${GROUP_HINT[g]}</span>` : ""}</div>
<div class="cards">${list.map((p) => card(p, { admin: true })).join("")}</div></section>`;
  })
  .join("")}`;
  return page({ file: "01-roster-cards.html", title: "Members roster", crumbs: "<b>Members</b>", body, note: "Tenant admin's People tab in cards: Requests above, then the MH-35 groups by highest level, each with a count and a one-line meaning. Status (Invited, Suspended) decides the group before level. Square cards, initials in a hairline square, level as a tag; the only sand fill is Invite people." });
}

function rosterListDev() {
  VIEWER = "Lena Kowalski";
  // Lena sees Billing app and Data warehouse only: levels count over those (R1); Aiden lands in Other projects.
  const vis = (p) => {
    if (p.lvl === "tenant_admin" || p.lvl === "invited" || p.lvl === "suspended" || p.lvl === "none") return p;
    const on = Object.entries(p.on ?? {}).filter(([k]) => k !== "ops");
    if (on.length === 0) return { ...p, lvl: "other", proj: 0 };
    const rank = { admin: 3, developer: 2, viewer: 1 };
    const top = on.map(([, r]) => r).sort((a, b) => rank[b] - rank[a])[0];
    return { ...p, lvl: top, proj: on.length };
  };
  const vp = people.map((p) => ({ ...vis(p), you: p.id === "lena" }));
  const body = `${membersHead("people", false)}${toolbar({ count: "14 people", view: "list", admin: false })}
<table class="list"><thead><tr><th>Person</th><th>Level</th><th>Projects</th><th>Status</th><th>Joined</th><th></th></tr></thead>
<tbody>${GROUPS.map(([g, label]) => {
    const list = vp.filter((p) => p.lvl === g);
    if (!list.length) return "";
    return `<tr class="gh"><td colspan="6"><h2>${label}</h2><span class="n">${list.length}</span>${GROUP_HINT[g] ? `<span class="dim note hide-sm" style="float:right">${GROUP_HINT[g]}</span>` : ""}</td></tr>${list.map((p) => row(p, { admin: false, levelOverride: g === "other" ? "other" : undefined })).join("")}`;
  }).join("")}</tbody></table>`;
  return page({ file: "02-roster-list-developer.html", title: "Members roster list", crumbs: "<b>Members</b>", body, note: "A Developer's view in list: read-only (no Invite, no row menus, no Requests, no Activity tab). Levels and project counts cover only projects Lena can see (R1), so Aiden, who works only on Ops console, sits under Other projects (MH-35). Below 40rem the rows stack." });
}

function rosterProject() {
  VIEWER = "Mara Okafor";
  const on = (p) => (p.lvl === "tenant_admin" ? "admin" : p.on?.bill);
  const groups = [
    ["admin", "Admins on Billing app"],
    ["developer", "Developers on Billing app"],
    ["viewer", "Viewers on Billing app"],
    ["noacc", "No access on Billing app"],
    ["invited", "Invited"],
    ["suspended", "Suspended"],
  ];
  const groupOf = (p) => (p.lvl === "invited" || p.lvl === "suspended" ? p.lvl : on(p) ?? "noacc");
  const body = `${membersHead()}${toolbar({ count: "14 people · 6 on Billing app", view: "list", admin: true, project: "Billing app" })}
<table class="list"><thead><tr><th>Person</th><th>On Billing app</th><th>Projects</th><th>Status</th><th>Joined</th><th></th></tr></thead>
<tbody>${groups
    .map(([g, label]) => {
      const list = people.filter((p) => groupOf(p) === g);
      if (!list.length) return "";
      return `<tr class="gh"><td colspan="6"><h2>${label}</h2><span class="n">${list.length}</span></td></tr>${list
        .map((p) =>
          row(p, {
            admin: true,
            levelOverride: p.lvl === "tenant_admin" ? "tenant_admin" : g === "noacc" ? null : p.lvl === "invited" ? null : on(p) === "admin" ? "admin" : on(p),
          }),
        )
        .join("")}`;
    })
    .join("")}</tbody></table>`;
  return page({ file: "03-roster-project-filter.html", title: "Members by project", crumbs: "<b>Members</b>", body, note: "The project filter regroups everyone by their role on Billing app: Admins (tenant admins marked, they're Admin everywhere), Developers, Viewers, then No access on Billing app. Invited and Suspended stay status groups. The Level column becomes the role on that project." });
}

// ---------------------------------------------------------------- member page
const BILL_ROLES = (mode) => {
  // mode: "admin" editable, "self" read-only
  const ro = mode === "self";
  const rowc = (state, name, gives, right = "", cls = "") =>
    `<div class="ci ${cls}">${cb(ro ? `${state} dis` : state, name)}<span class="rn">${rn(name)}</span>${gives ? `<span class="gives">also gives <span class="mono">${gives}</span></span>` : ""}<span class="r">${right}</span></div>`;
  return `<div class="check" role="group" aria-label="Postgres roles on Billing app">
  ${ro ? "" : `<div class="cf"><span class="field">${I("search", "s16 dim")}<span class="ph">Filter 9 roles</span></span></div>`}
  ${rowc("on", "billing_editor", "billing_reader")}
  ${rowc("on", "billing_reader", "")}
  <div class="ci locked">${cb("on dis", "support_agent")}<span class="rn">support_agent</span><span class="r">${I("lock", "s12")}<span>via <a href="#">Support team</a></span><span class="tag">step 2</span></span></div>
  ${rowc("", "reports_reader", "", "only on billing-prod", "off")}
  ${rowc("", "refunds‮review", "", "only on billing-prod", "off")}
  ${rowc("", "webhook_worker", "billing_reader", "", "off")}
  ${ro ? "" : `<div class="sub">Admin only</div>
  ${rowc("dis", "billing_admin", "", "Reaches <span class=\"mono\">billing_owner</span>, which is owner-level", "off")}
  ${rowc("dis", "migrator", "", "LOGIN role", "off")}
  ${rowc("dis", "audit_bypass", "", "Bypasses RLS", "off")}`}
</div>`;
};
const WH_ROLES = (mode) => {
  const ro = mode === "self";
  const r = (state, name, gives, cls = "") => `<div class="ci ${cls}">${cb(ro ? `${state} dis` : state, name)}<span class="rn">${name}</span>${gives ? `<span class="gives">also gives <span class="mono">${gives}</span></span>` : ""}</div>`;
  return `<div class="check" role="group" aria-label="Postgres roles on Data warehouse">
  ${r("on", "reports_reader", "")}
  ${ro ? r("on", "etl_writer", "staging_reader") : r("on", "etl_writer", "staging_reader", "pend")}
  ${r("", "staging_reader", "", "off")}
  ${r("", "analyst", "reports_reader", "off")}
</div>`;
};
const segRole = (on, ro, opts = ["No access", "Viewer", "Developer", "Admin"]) => `<div class="seg${ro ? " dis" : ""}" role="radiogroup" aria-label="Platform role">${opts.map((x) => `<span role="radio" aria-checked="${x === on}" class="${x === on ? "on" : ""}">${x}</span>`).join("")}</div>`;
const ROLE_LINE = {
  Developer: "Runs SQL and edits data as the roles below, shares saved queries, manages app users and unprotected branches.",
  Viewer: "Reads tables and structure as the roles below. No SQL editor, no writes, no connection strings or keys.",
};

function grantCards(mode, who = "Sofia") {
  const ro = mode === "self";
  return `
<div class="gcard">
  <div class="gh"><h3>Billing app</h3><span class="dim note">billing-prod, billing-dev</span></div>
  <div class="gb">
    <div><div class="flabel">Platform role</div>${segRole("Developer", ro)}<p class="note dim" style="margin-top:6px">${ROLE_LINE.Developer}</p></div>
    <div><div class="flabel">Postgres roles <span>${ro ? "Roles you may act as" : "Checked roles come first; a role's chain comes with it"}</span></div>${BILL_ROLES(mode)}</div>
    <div class="disclose">${I("right", "s16")}<b>Narrow on a database</b><span>Same on billing-prod and billing-dev</span></div>
  </div>
  <div class="gf"><span class="eff">Effective: <b>Developer</b> · <span class="mono">billing_editor, billing_reader</span> (+ <span class="mono">support_agent</span> via Support team)</span>${ro ? "" : `<button class="btn sm danger">Take access away</button><button class="btn sm primary" disabled>Save</button>`}</div>
</div>
<div class="gcard${ro ? "" : " dirty"}">
  <div class="gh"><h3>Data warehouse</h3><span class="dim note">warehouse</span>${ro ? "" : `<span style="margin-left:auto" class="line"><span class="sq sand"></span>Unsaved changes</span>`}</div>
  <div class="gb">
    <div><div class="flabel">Platform role</div>${segRole("Developer", ro)}</div>
    <div><div class="flabel">Postgres roles</div>${WH_ROLES(mode)}</div>
    <div class="disclose">${I("right", "s16")}<b>Narrow on a database</b><span>Narrowed on warehouse: Viewer, <span class="mono">reports_reader</span> only</span></div>
  </div>
  <div class="gf"><span class="eff">Effective: <b>Developer</b> · <span class="mono">reports_reader, etl_writer</span> (+ <span class="mono">staging_reader</span>)</span>${ro ? "" : `<button class="btn sm ghost">Discard</button><button class="btn sm primary">Save</button>`}</div>
</div>
${ro ? `<p class="note dim" style="margin-top:12px">Ask a tenant admin or a project Admin to change your access.</p>` : `<div class="gcard" style="border-style:dashed;border-color:var(--line-strong)">
  <div class="gh add-row" style="border:0"><h3 style="font-weight:500">Add access to a project</h3><span class="dim note">${who} has no access on Ops console</span>
  <span style="margin-left:auto;display:flex;gap:8px"><span class="field select" style="width:180px;height:32px"><span class="v">Ops console</span>${I("down", "s16 dim")}</span><button class="btn outline sm">${I("plus", "s16")}Add</button></span></div>
</div>`}`;
}

function loginsTable(mode) {
  const self = mode === "self";
  const rows = [
    { db: "billing-prod", proj: "Billing app", name: "sofia", exp: '<span class="line"><span class="sq coral-o"></span>in 9 days · 1 Nov</span>', created: "3 Aug 2026", soon: true },
    { db: "warehouse", proj: "Data warehouse", name: "sofia_m", exp: "in 71 days · 2 Jan 2027", created: "23 Oct 2026" },
    { db: "ops", proj: "Ops console", name: "sofia", exp: '<span class="dim">Revoked 12 Oct: access to Ops console was taken away</span>', created: "6 Jun 2026", revoked: true },
  ];
  return `<div class="wrap"><table class="grid"><thead><tr><th>Database</th><th>Login</th><th>Expires</th><th class="hide-sm">Created</th><th></th></tr></thead><tbody>
${rows
  .map(
    (r) => `<tr><td>${r.revoked ? `<span class="dim">${r.db}</span>` : r.db}<div class="dim label">${r.proj}</div></td><td class="mono${r.revoked ? " dim" : ""}">${r.name}</td><td class="note">${r.exp}</td><td class="hide-sm dim note">${r.created}</td>
<td style="text-align:right;white-space:nowrap">${
      r.revoked
        ? `<button class="btn sm ghost">Delete</button>`
        : self
          ? `${r.soon ? `<button class="btn sm outline">${I("renew", "s14")}Renew</button>` : `<button class="btn sm ghost">Reset password</button>`}<button class="btn sm ghost">Delete</button>`
          : `<button class="btn sm ghost">Delete</button>`
    }</td></tr>`,
  )
  .join("")}
</tbody></table></div>
${self ? `<div class="info" style="margin-top:10px"><span class="sq coral-o" style="margin-top:6px"></span><p class="note"><b style="font-weight:600">sofia on billing-prod expires on 1 Nov.</b> Renew it for 90 more days: you get a new password, and the old one stops working.</p><button class="btn sm outline" style="margin-left:auto">Renew</button></div>` : `<p class="note dim" style="margin-top:10px">Only Sofia can create, reset or renew her logins. Expiring soon: <span class="mono">sofia</span> on billing-prod, 1 Nov.</p>`}
<p class="note dim" style="margin-top:8px">A login works on every branch of its database where ${self ? "you have" : "Sofia has"} Developer access or more. Connection strings are for the main branch; on another branch, use that branch's host.</p>`;
}

const activity = `<div class="events">
  <div class="ev"><time title="2026-10-23 09:14 UTC">2 hours ago</time><span>Sofia Marchetti created the personal login <span class="mono">sofia_m</span> on warehouse</span>${I("down", "s16 dim")}</div>
  <div class="ev"><time title="2026-10-12 16:40 UTC">12 Oct</time><span>Priya Raman took Sofia Marchetti's access to Ops console away; her login <span class="mono">sofia</span> on ops was revoked</span>${I("down", "s16 dim")}</div>
  <div class="ev"><time title="2026-10-02 11:05 UTC">2 Oct</time><span>Mara Okafor changed Sofia Marchetti's access on Data warehouse: Viewer → Developer</span>${I("down", "s16 dim")}</div>
  <div class="ev"><time title="2026-08-03 08:22 UTC">3 Aug</time><span>Sofia Marchetti created the personal login <span class="mono">sofia</span> on billing-prod</span>${I("down", "s16 dim")}</div>
</div><button class="btn ghost sm" style="margin-top:8px">Show older</button>`;

function memberHeader(mode) {
  const self = mode === "self";
  return `<a class="back" href="01-roster-cards.html">${I("back", "s16")}Members</a>
<div class="mhead">
  <div class="id"><span class="av lg">SM</span><div style="min-width:0">
    <h1>Sofia Marchetti${self ? ' <span class="dim" style="font-weight:400">(you)</span>' : ""}</h1>
    <div class="facts"><span>sofia@northwind.dev</span>${lv("developer")}<span>Active</span><span>Joined 9 May 2026</span></div>
  </div></div>
  <div class="acts">${
    self
      ? ""
      : `<button class="btn outline">Make tenant admin</button><button class="btn outline">${I("key", "s16")}Revoke Postgres access now</button><button class="btn danger">Suspend</button><button class="btn icon outline" aria-label="More actions for Sofia Marchetti">${I("more", "s16")}</button>`
  }</div>
</div>`;
}

function memberPage(mode) {
  const self = mode === "self";
  VIEWER = self ? "Sofia Marchetti" : "Mara Okafor";
  const body = `${memberHeader(mode)}
<section class="section"><div class="sh"><div><h2>Project access</h2><p class="note dim">${self ? "What you can reach, project by project." : "Each project's platform role and the Postgres roles Sofia may act as. Each card saves on its own."}</p></div></div>
${grantCards(mode)}</section>
<section class="section"><div class="sh"><div><h2>Personal Postgres logins</h2><p class="note dim">${self ? "Your own login per database, for psql and other clients. It has exactly the roles your access gives you." : "Sofia's own logins, one per database, for psql and other clients."}</p></div>${self ? `<button class="btn outline">${I("plus", "s16")}New login</button>` : ""}</div>
${loginsTable(mode)}</section>
${self ? "" : `<section class="section"><div class="sh"><div><h2>Activity</h2><p class="note dim">Changes to Sofia's access made through Chelabase.</p></div></div>${activity}</section>`}`;
  return page({
    file: self ? "05-member-self.html" : "04-member-admin-view.html",
    title: self ? "Your member page" : "Member page",
    crumbs: `<span>Members</span>${I("right", "s14")}<b>Sofia Marchetti</b>`,
    body,
    note: self
      ? "Sofia's own page: project access is read-only (no Save, roles listed but disabled, no Admin-only group), personal logins are hers to manage. Her billing-prod login expires in 9 days, so the row offers Renew and a prompt names the date and what renewing does (new password, 90 more days)."
      : "A tenant admin on Sofia's page. Header actions by power (Suspend and Revoke Postgres access now are tenant-admin only, Remove from tenant in the overflow). One card per project with the segmented platform role and the filterable role checklist: checked first, 'also gives' chains, the escaped bidi name refunds\\u202Ereview, the Admin-only group with reasons, and step 2's locked 'via team' row (lock + link, never sent in the PUT). The warehouse card shows unsaved edits (sand pending edge, its own Save). Logins: Admins see Delete only; expiring soon is a coral-outlined square.",
  });
}

function memberSuspended() {
  VIEWER = "Mara Okafor";
  const body = `<a class="back" href="01-roster-cards.html">${I("back", "s16")}Members</a>
<div class="mhead">
  <div class="id"><span class="av lg">FB</span><div style="min-width:0">
    <h1>Felix Bauer</h1>
    <div class="facts"><span>felix.bauer@northwind.dev</span>${lv("developer")}<span class="coral">Suspended</span><span>Joined 6 May 2026</span></div>
  </div></div>
  <div class="acts"><button class="btn primary">Reinstate</button><button class="btn icon outline" aria-label="More actions for Felix Bauer">${I("more", "s16")}</button></div>
</div>
<div class="alert" style="margin-top:20px"><span class="sq coral-o"></span><div><p><b style="font-weight:600">Suspended on 14 Oct by Mara Okafor.</b> Felix can't sign in to Studio, his personal logins were revoked and his database sessions ended.</p><p class="note dim" style="margin-top:4px">His project access below is kept. Reinstate gives it back as it is; he makes new personal logins himself.</p></div></div>
<section class="section"><div class="sh"><div><h2>Project access</h2><p class="note dim">Kept while suspended. Changes apply when Felix is reinstated.</p></div></div>
<div class="gcard"><div class="gh"><h3>Billing app</h3><span class="dim note">billing-prod, billing-dev</span></div>
<div class="gb"><div><div class="flabel">Platform role</div>${segRole("Developer")}</div><div class="disclose">${I("right", "s16")}<b>Postgres roles</b><span class="mono">billing_editor, billing_reader</span></div></div>
<div class="gf"><span class="eff">Effective once reinstated: <b>Developer</b> · <span class="mono">billing_editor, billing_reader</span></span><button class="btn sm primary" disabled>Save</button></div></div>
<div class="gcard"><div class="gh"><h3>Ops console</h3><span class="dim note">ops</span></div>
<div class="gb"><div><div class="flabel">Platform role</div>${segRole("Viewer")}</div><div class="disclose">${I("right", "s16")}<b>Postgres roles</b><span class="mono">ops_reader</span></div></div>
<div class="gf"><span class="eff">Effective once reinstated: <b>Viewer</b> · <span class="mono">ops_reader</span></span><button class="btn sm primary" disabled>Save</button></div></div>
</section>
<section class="section"><div class="sh"><div><h2>Personal Postgres logins</h2></div></div>
<div class="wrap"><table class="grid"><thead><tr><th>Database</th><th>Login</th><th>Status</th><th></th></tr></thead><tbody>
<tr><td>billing-prod<div class="dim label">Billing app</div></td><td class="mono dim">felix</td><td class="note dim">Revoked 14 Oct: suspended</td><td style="text-align:right"><button class="btn sm ghost">Delete</button></td></tr>
<tr><td>billing-dev<div class="dim label">Billing app</div></td><td class="mono dim">felix</td><td class="note dim">Revoked 14 Oct: suspended</td><td style="text-align:right"><button class="btn sm ghost">Delete</button></td></tr>
</tbody></table></div></section>`;
  return page({ file: "06-member-suspended.html", title: "Suspended member", crumbs: `<span>Members</span>${I("right", "s14")}<b>Felix Bauer</b>`, body, note: "A suspended member: the status is coral, a coral-bordered box says when, by whom and what stopped, and Reinstate is the page's one sand action. Grants stay visible and editable ('effective once reinstated'); revoked logins stay listed with their reason until deleted (P6)." });
}

// ---------------------------------------------------------------- roles tab
const ROLES = [
  { n: "billing_editor", attrs: ["NOLOGIN"], m: 4, sum: "SELECT, INSERT, UPDATE on 6 tables in billing" },
  { n: "billing_reader", attrs: ["NOLOGIN"], m: 7, sum: "SELECT on 9 tables in billing, public" },
  { n: "support_agent", attrs: ["NOLOGIN"], m: 3, sum: "SELECT on customers, invoices · UPDATE on 2 columns" },
  { n: "reports_reader", attrs: ["NOLOGIN"], m: 5, sum: "SELECT on 4 views in reports · in authenticated's chain", flag: true },
  { n: "webhook_worker", attrs: ["NOLOGIN"], m: 0, sum: "EXECUTE on 2 functions in billing" },
  { n: "refunds‮review", attrs: ["NOLOGIN"], m: 0, sum: "No privileges yet" },
  { n: "billing_admin", attrs: ["NOLOGIN"], owner: true, m: 1, sum: "Member of billing_owner" },
  { n: "migrator", attrs: ["LOGIN"], m: 0, sum: "Owns 3 tables in billing" },
];
function rolesTab() {
  VIEWER = "Mara Okafor";
  const body = `${membersHead("roles")}
<div class="pickers">
  ${select("Project", "Billing app")}${select("Database", "billing-prod")}${select("Branch", "main · protected")}${select("Postgres database", "billing")}
  <button class="btn primary">${I("plus", "s16")}Create role</button>
</div>
<div class="info" style="margin-top:14px">${I("lock", "s16 dim")}<p class="note">main is protected: role and privilege changes here need Admin, and Developers' logins can't change the schema. Roles live on this branch only; to have one elsewhere, merge or reset the other branch.</p></div>
<div class="toolbar" style="margin-top:20px"><span class="count">8 roles on main</span><span class="field">${I("search", "s16 dim")}<span class="ph">Filter roles</span></span>${viewToggle("cards")}</div>
<div class="rcards">${ROLES.map(
    (r) => `<a class="rcard" href="08-role-page.html"><div class="rn">${rn(r.n)}</div>
<div class="meta">${r.attrs.map((a) => `<span class="tag">${a}</span>`).join("")}${r.owner ? '<span class="tag coral">owner-level</span>' : ""}<span>· ${r.m === 0 ? "No members" : `${r.m} member${r.m > 1 ? "s" : ""}`}</span></div>
<div class="sum">${r.sum}</div></a>`,
  ).join("")}</div>
<section class="section" style="margin-top:32px"><div class="sh"><div><h2>Personal logins on billing-prod <span class="dim" style="font-weight:400">3</span></h2><p class="note dim">Members' own logins. They aren't roles you grant: each follows its member's access, on every branch.</p></div></div>
<div class="wrap"><table class="grid"><thead><tr><th>Login</th><th>Member</th><th>Expires</th></tr></thead><tbody>
<tr><td class="mono">priya</td><td><a href="#">Priya Raman</a></td><td class="note">in 64 days</td></tr>
<tr><td class="mono">sofia</td><td><a href="04-member-admin-view.html">Sofia Marchetti</a></td><td class="note"><span class="line"><span class="sq coral-o"></span>in 9 days</span></td></tr>
<tr><td class="mono">lena_k</td><td><a href="#">Lena Kowalski</a></td><td class="note">in 88 days</td></tr>
</tbody></table></div></section>`;
  return page({ file: "07-roles-tab.html", title: "Roles tab", crumbs: "<b>Members</b>", body, note: "The Roles tab for an Admin: project → database → branch → Postgres database pickers (search params), the protected-branch note, role cards with NOLOGIN/LOGIN, owner-level in coral, member counts (Admins only, MH-34; a Developer sees the same cards without counts and without the logins list), a privilege summary, and the escaped name. Personal logins are never role cards: they get their own Admin-only list (P24) linking to each member." });
}

// ---------------------------------------------------------------- role page + privilege editor
const PRIVS = ["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER", "MAINTAIN"];
function mxRow(name, kind, set, extra = {}) {
  const cells = PRIVS.map((p, i) => {
    const s = set[i];
    if (extra.grey) return `<td>${cb(s ? "on dis" : "dis")}</td>`;
    const pend = extra.pend?.includes(i);
    const go = extra.go?.includes(i);
    return `<td class="${pend ? "pend" : ""}"><span class="${go ? "go" : ""}">${cb(s ? "on" : "", `${p} on ${name}`)}</span></td>`;
  }).join("");
  const all = set.every(Boolean) ? "on" : set.some(Boolean) ? "mixed" : "";
  return `<tr class="${extra.grey ? "grey" : ""}"><td><div class="on-name">${extra.grey ? I("lock", "s12 dim") : `<button class="btn icon sm ghost" aria-label="Columns of ${name}" aria-expanded="${extra.open ? "true" : "false"}">${I(extra.open ? "down" : "right", "s14")}</button>`}<span class="mono">${name}</span>${kind ? `<span class="tag">${kind}</span>` : ""}</div></td>${cells}<td>${extra.grey ? "" : cb(extra.grey ? "dis" : all, `All privileges on ${name}`)}</td><td style="min-width:220px">${extra.right ?? ""}</td></tr>`;
}
function privEditor() {
  const T = (s) => s.split("").map((c) => c === "1");
  return `
<div class="schema">
  <div class="sh">${I("down", "s16 dim")}<span class="mono">billing</span><span class="dim note">schema · 7 tables and views · 1 sequence · 3 functions</span></div>
  <div class="blk"><div class="bh"><h3>Schema</h3><span class="d">Without USAGE, nothing inside billing is reachable.</span></div>
    <div class="privrow"><label>${cb("on")}USAGE</label><label>${cb("")}CREATE</label><span class="grantor">USAGE granted by postgres</span></div></div>
  <div class="blk" style="padding:0"><div class="bh" style="padding:14px 14px 0"><h3>Tables and views</h3><span class="d">G marks a privilege held with grant option.</span></div>
    <div style="overflow-x:auto;margin-top:10px"><table class="mx" aria-label="Table privileges of billing_editor in billing">
      <thead><tr><th>Object</th>${PRIVS.map((p) => `<th>${p}<br>${cb(p === "SELECT" ? "mixed" : p === "INSERT" || p === "UPDATE" ? "mixed" : "", `${p} on every table`)}</th>`).join("")}<th>All</th><th>Notes</th></tr></thead>
      <tbody>
        ${mxRow("invoices", "", T("11100000"), { go: [0], right: '<span class="grantor">SELECT with grant option · <a href="#">Revoke grant option</a></span>' })}
        ${mxRow("invoice_lines", "", T("11100000"))}
        ${mxRow("customers", "", T("10010000"), { pend: [3], open: true, right: '<span class="grantor">2 column grants</span>' })}
        <tr class="drill"><td colspan="11"><div class="label dim" style="margin-bottom:8px">Column privileges on <span class="mono">customers</span>. A table-wide privilege covers every column.</div>
          <div class="cols"><div class="hd">column</div><div class="hd">SELECT</div><div class="hd">INSERT</div><div class="hd">UPDATE</div><div class="hd">REFERENCES</div>
          <div>id</div><div>${cb("on dis")}</div><div>${cb("")}</div><div>${cb("")}</div><div>${cb("")}</div>
          <div>email</div><div>${cb("on dis")}</div><div>${cb("")}</div><div>${cb("on")}</div><div>${cb("")}</div>
          <div>display_name</div><div>${cb("on dis")}</div><div>${cb("")}</div><div class="pend">${cb("on")}</div><div>${cb("")}</div>
          <div>tax_id</div><div>${cb("on dis")}</div><div>${cb("")}</div><div>${cb("")}</div><div>${cb("")}</div></div></td></tr>
        ${mxRow("payments", "", T("10000000"), { grey: true, right: `<span class="ownedby">owned by <span class="mono">ledger_svc</span>: change in SQL as <span class="mono">ledger_svc</span></span>` })}
        ${mxRow("plans", "", T("10000000"), { right: '<span class="grantor">SELECT granted by <span class="mono">neon_superuser</span></span>' })}
        ${mxRow("refunds", "", T("11000000"), { pend: [1] })}
        ${mxRow("open_balances", "view", T("10000000"))}
      </tbody></table></div></div>
  <div class="blk"><div class="bh"><h3>All rows</h3><span class="d">Tables with row-level security on. On, billing_editor sees and changes every row through a policy Chelabase adds.</span></div>
    <div class="rls"><div class="mono">invoices</div><div class="dim note">off</div><div>${'<span class="switch" role="switch" aria-checked="false" aria-label="All rows on invoices"></span>'}</div>
    <div class="mono">customers</div><div class="note sea">on: policy billing_editor_all_rows</div><div><span class="switch on" role="switch" aria-checked="true" aria-label="All rows on customers"></span></div>
    <div class="mono">refunds</div><div class="note">on, staged</div><div><span class="switch on pend" role="switch" aria-checked="true" aria-label="All rows on refunds"></span></div></div>
    <p class="note dim" style="margin-top:8px">Restrictive policies still apply, and a role without SELECT on a table sees nothing there. <a href="#">All rows on every RLS table in billing</a></p></div>
  <div class="blk"><div class="bh"><h3>Sequences</h3></div>
    <div class="privrow"><span class="mono" style="width:180px">invoices_id_seq</span><label>${cb("on")}USAGE</label><label>${cb("on")}SELECT</label><label>${cb("")}UPDATE</label></div></div>
  <div class="blk"><div class="bh"><h3>Functions and procedures</h3></div>
    <div style="display:grid;gap:8px">
      <div class="privrow"><span class="mono" style="min-width:300px">issue_refund(uuid, numeric)</span><label>${cb("on")}EXECUTE</label></div>
      <div class="privrow"><span class="mono" style="min-width:300px">recalc_totals(uuid)</span><label style="background:var(--sand-wash);box-shadow:inset 0 2px 0 rgb(242 199 163 / .45);padding:2px 6px">${cb("on")}EXECUTE</label></div>
      <div class="privrow"><span class="mono dim" style="min-width:300px">${I("lock", "s12")} close_period(date)</span><label class="dim">${cb("dis")}EXECUTE</label><span class="ownedby">owned by <span class="mono">ledger_svc</span>: change in SQL as <span class="mono">ledger_svc</span></span></div>
    </div></div>
  <div class="blk"><div class="bh"><h3>Future objects</h3><span class="d">Default privileges for objects created later in billing.</span></div>
    <div style="display:flex;gap:10px;align-items:center;flex-wrap:wrap;margin-bottom:10px"><span class="note">Created by</span><span class="field select" style="width:200px;height:32px"><span class="v mono">postgres (owner)</span>${I("down", "s16 dim")}</span></div>
    <div style="display:grid;gap:8px">
      <div class="privrow"><span style="width:120px">New tables</span><label>${cb("on")}SELECT</label><label>${cb("on")}INSERT</label><label>${cb("on")}UPDATE</label><label>${cb("")}DELETE</label></div>
      <div class="privrow"><span style="width:120px">New sequences</span><label>${cb("on")}USAGE</label><label>${cb("")}SELECT</label><label>${cb("")}UPDATE</label></div>
      <div class="privrow"><span style="width:120px">New functions</span><label>${cb("")}EXECUTE</label></div>
    </div></div>
</div>
<div class="schema closed"><div class="sh">${I("right", "s16 dim")}<span class="mono">public</span><span class="dim note">USAGE · SELECT on 2 tables</span></div></div>`;
}
function roleHeader(name, extra = "") {
  return `<a class="back" href="07-roles-tab.html">${I("back", "s16")}Roles</a>
<div class="rhead"><div style="flex:1;min-width:0">
  <div class="ctx">Billing app ${I("right", "s12")} billing-prod ${I("right", "s12")} main <span class="tag">protected</span> ${I("right", "s12")} Postgres database <span class="mono">billing</span></div>
  <h1 style="margin-top:6px">${name}</h1>
  <div class="attrs"><span class="tag">NOLOGIN</span><span class="tag">INHERIT</span><span class="tag">no BYPASSRLS</span><span class="tag">no connection limit</span></div>
  <p class="note dim" style="margin-top:8px">This branch only: to have it elsewhere, merge or reset the other branch.</p>
</div>${extra}</div>
<div class="subtabs" role="tablist"><span class="on">Privileges</span><span>Members <span class="dim">4</span></span><span>Memberships</span><span>Danger zone</span></div>`;
}
function rolePage() {
  VIEWER = "Priya Raman";
  const panel = `<aside class="panel" aria-label="Pending changes">
  <div class="ph"><h2>Pending changes · 4</h2><span class="collapse">${I("pclose", "s16")}</span></div>
  <p class="note dim">Nothing is applied yet. Review runs a dry run and shows the SQL.</p>
  <div class="edits">
    <div class="edit"><span class="n">1</span><span>GRANT DELETE ON billing.customers</span><span class="x">${I("x", "s14")}</span></div>
    <div class="edit"><span class="n">2</span><span>GRANT UPDATE (display_name) ON billing.customers</span><span class="x">${I("x", "s14")}</span></div>
    <div class="edit"><span class="n">3</span><span>GRANT INSERT ON billing.refunds</span><span class="x">${I("x", "s14")}</span></div>
    <div class="edit"><span class="n">4</span><span>All rows on billing.refunds</span><span class="x">${I("x", "s14")}</span></div>
    <div class="edit"><span class="n">5</span><span>GRANT EXECUTE ON billing.recalc_totals(uuid)</span><span class="x">${I("x", "s14")}</span></div>
  </div>
  <div class="foot"><button class="btn ghost">Discard</button><button class="btn primary">Review changes</button></div>
</aside>`;
  const layout = `<div class="rp"><div class="work"><div class="page wide">${roleHeader("billing_editor")}${privEditor()}</div></div>${panel}</div>`;
  return page({ file: "08-role-page.html", title: "Role privileges", crumbs: `<span>Members</span>${I("right", "s14")}<span>Roles</span>${I("right", "s14")}<b class="mono">billing_editor</b>`, layout, note: "A project Admin on billing_editor: one schema block per schema with Schema, Tables and views (objects × 8 privileges, 'all' per row and column), the column drill-in open on customers, All rows with its caveat line, Sequences, Functions, Future objects. Staged cells carry the sand pending edge; the right panel collects them (one panel, one sand: Review changes). payments and close_period are owned by ledger_svc, which the owner can't SET to: greyed with a lock and 'change in SQL as ledger_svc'. Grantors show where they matter (neon_superuser, P30); grant option is shown and only revocable (MH-29)." });
}

function roleReview(state) {
  VIEWER = "Priya Raman";
  const conflict = state === "conflict";
  const panel = `<aside class="panel" aria-label="Review changes">
  <div class="ph"><h2>${conflict ? "Not applied" : "Review · 3 changes"}</h2><span class="collapse">${I("pclose", "s16")}</span></div>
  ${
    conflict
      ? `<div class="alert"><span class="sq coral"></span><div class="note"><p class="coral" style="font-weight:600">Error: nothing was applied.</p><p style="margin-top:4px">SELECT on <span class="mono">reports.monthly_revenue</span> is still held by <span class="mono">reports_reader</span>: it was granted by <span class="mono">analytics_owner</span>, so revoking it as the owner left it in place.</p><p class="dim" style="margin-top:6px">Revoke it in SQL as <span class="mono">analytics_owner</span>, or leave SELECT checked. The whole change rolled back.</p></div></div>
      <div class="line dim"><span class="sq" style="border:1.5px solid var(--chalk-dim)"></span><span>If someone else changes these privileges first, you'll be asked to reload instead: "Privileges on reports.daily_signups changed since you opened this page. Reload to see them; your 3 changes are kept to re-apply."</span></div>`
      : `<div class="line"><span class="sq coral-o"></span><span><b style="font-weight:600">Exposes data through the data API.</b> <span class="mono">authenticated</span> inherits <span class="mono">reports_reader</span>, so every signed-in app user gets these grants.</span></div>
  <div class="alert"><span class="sq coral"></span><div class="note"><p class="coral" style="font-weight:600">Error: All rows can't be turned on for reports_reader.</p><p style="margin-top:4px">It would expose every row of <span class="mono">reports.daily_signups</span> to the data API: <span class="mono">reports_reader ← authenticated</span>.</p><p class="dim" style="margin-top:6px">Remove change 3, or take reports_reader out of authenticated first.</p></div></div>`
  }
  <div class="sql"><span class="k">-- dry run on main, Postgres database billing</span>
GRANT SELECT ON reports.daily_signups TO reports_reader;
GRANT SELECT ON reports.weekly_churn TO reports_reader;
<span class="hl">REVOKE SELECT ON reports.monthly_revenue FROM reports_reader;</span>${conflict ? "" : `
<span class="k">-- all rows: refused, not run</span>`}</div>
  ${conflict ? "" : `<div class="line sea"><span class="sq sea"></span>Dry run passed for changes 1 and 2 · no table lock</div>`}
  <div class="foot"><button class="btn ghost">${conflict ? "Back to editing" : "Back"}</button><button class="btn primary"${conflict ? "" : " disabled"}>${conflict ? "Reload privileges" : "Apply 3 changes"}</button></div>
</aside>`;
  const work = `<div class="page wide">${roleHeader("reports_reader")}
<div class="schema"><div class="sh">${I("down", "s16 dim")}<span class="mono">reports</span><span class="dim note">schema · 4 views</span></div>
<div class="blk" style="padding:0"><div style="overflow-x:auto"><table class="mx"><thead><tr><th>Object</th>${PRIVS.map((p) => `<th>${p}</th>`).join("")}<th>All</th><th>Notes</th></tr></thead><tbody>
${mxRow("daily_signups", "view", [1, 0, 0, 0, 0, 0, 0, 0], { pend: [0] })}
${mxRow("weekly_churn", "view", [1, 0, 0, 0, 0, 0, 0, 0], { pend: [0] })}
${mxRow("monthly_revenue", "view", [0, 0, 0, 0, 0, 0, 0, 0], { pend: [0], right: '<span class="grantor">SELECT granted by <span class="mono">analytics_owner</span></span>' })}
${mxRow("arr_by_plan", "view", [1, 0, 0, 0, 0, 0, 0, 0])}
</tbody></table></div></div>
<div class="blk"><div class="bh"><h3>All rows</h3></div><div class="rls"><div class="mono">daily_signups</div><div class="note">on, staged</div><div><span class="switch on pend"></span></div></div></div></div></div>`;
  const layout = `<div class="rp"><div class="work">${work}</div>${panel}</div>`;
  return page({
    file: conflict ? "10-role-conflict.html" : "09-role-review.html",
    title: conflict ? "Privileges not applied" : "Review privilege changes",
    crumbs: `<span>Members</span>${I("right", "s14")}<span>Roles</span>${I("right", "s14")}<b class="mono">reports_reader</b>`,
    layout,
    note: conflict
      ? "After Apply, the server's verify-after check (MH-24) found SELECT still held through another grantor: 409 privileges_not_applied, named object, privilege and grantor, everything rolled back. The staleness 409 (privileges_changed_since_read) is a different message: reload, keep the staged changes."
      : "Review changes: the dry-run SQL, the exposure flag (authenticated inherits reports_reader), and the dry run's 422 exposes_data_api on All rows with the server's chain. Apply stays disabled until the refused change is removed.",
  });
}

// ---------------------------------------------------------------- dialogs
function dialogOver(file, title, crumbs, behind, dialog, note, viewer = "Mara Okafor") {
  VIEWER = viewer;
  return page({ file, title, crumbs, body: behind, overlay: `<div class="backdrop"></div>${dialog}`, note });
}
function createRole() {
  const dlg = `<div class="dialog" role="dialog" aria-modal="true" aria-labelledby="cr-t">
  <div><div class="dt" id="cr-t">Create a role</div><p class="dd">On Billing app · billing-prod · main. This branch only.</p></div>
  <div class="fl"><span class="lbl">Name</span><span class="field focus mono">refund_auditor</span><span class="help">Lowercase letters, digits and _, starting with a letter or _. Up to 63 characters.</span></div>
  <div class="fl"><span class="lbl">Kind</span>
    <label style="display:flex;gap:10px;align-items:flex-start"><span class="radio on" style="margin-top:2px"></span><span><span>Group role (NOLOGIN)</span><span class="help dim label" style="display:block">Members act as it. Nobody signs in as it.</span></span></label>
    <label style="display:flex;gap:10px;align-items:flex-start"><span class="radio" style="margin-top:2px"></span><span><span>Login role</span><span class="label dim" style="display:block">An app or service signs in as it with a password.</span></span></label></div>
  <div class="disclose" style="border-top:1px solid var(--line);padding-top:12px">${I("down", "s16")}<b>Advanced</b></div>
  <div style="display:grid;gap:12px;padding-left:24px">
    <div class="fl"><span class="lbl" style="font-size:13px">Member of</span><div class="check"><div class="ci">${cb("on")}<span class="rn">billing_reader</span></div><div class="ci">${cb("")}<span class="rn">support_agent</span></div><div class="ci">${cb("")}<span class="rn">reports_reader</span><span class="r">in authenticated's chain</span></div></div></div>
    <label style="display:flex;gap:8px;align-items:center">${cb("on")}<span>Inherit privileges of the roles it's in</span></label>
    <label style="display:flex;gap:8px;align-items:center">${cb("")}<span>Bypass row-level security</span><span class="tag coral">owner-level</span></label>
    <div style="display:grid;grid-template-columns:1fr 1fr;gap:10px"><div class="fl"><span class="lbl" style="font-size:13px">Connection limit</span><span class="field"><span class="ph">No limit</span></span></div><div class="fl"><span class="lbl" style="font-size:13px">Valid until</span><span class="field"><span class="ph">Never</span></span></div></div>
  </div>
  <div class="df"><button class="btn ghost">Cancel</button><button class="btn primary">Create role</button></div>
</div>`;
  const behind = `${membersHead("roles")}<div class="pickers">${select("Project", "Billing app")}${select("Database", "billing-prod")}${select("Branch", "main · protected")}${select("Postgres database", "billing")}<button class="btn primary">Create role</button></div>`;
  return dialogOver("11-create-role.html", "Create role", "<b>Members</b>", behind, dlg, "Create role: name with the API's rule, NOLOGIN by default (spelled as 'Group role'), Advanced open: member of (with the data-API chain noted), inherit, bypass RLS tagged owner-level, connection limit, valid until. Owner-level creation stays in the branch Roles panel. After creating, Studio opens the new role's page.");
}
function loginReveal() {
  const dlg = `<div class="dialog wide" role="dialog" aria-modal="true" aria-labelledby="lr-t" style="width:38rem">
  <div><div class="dt" id="lr-t">Your login sofia_m on warehouse</div><p class="dd">Data warehouse · expires 21 Jan 2027 (90 days). Reset the password before then to renew it.</p></div>
  <div class="fl"><span class="lbl">Password</span><div class="secret"><span class="v">q7Hc2vR9xW4mTzK8pLs3NbE6yUa1</span><span class="c">${I("copy", "s14")}Copy</span></div></div>
  <div class="fl"><span class="lbl">Connection string <span class="dim" style="font-weight:400">· main branch</span></span><div class="secret"><span class="v">postgresql://sofia_m:q7Hc2vR9xW4mTzK8pLs3NbE6yUa1@ep-warehouse-main.db.northwind.example:5432/warehouse?sslmode=require</span><span class="c">${I("copy", "s14")}Copy</span></div>
  <span class="help">Works on every branch of warehouse where you have Developer access or more; on another branch, use that branch's host.</span></div>
  <div class="info"><p class="note">Your roles here: <span class="mono">reports_reader</span>, <span class="mono">etl_writer</span>. In psql, run <span class="mono">SET ROLE etl_writer;</span> to act as one.</p></div>
  <p style="font-weight:600">You won't see this password again. Copy it now.</p>
  <div class="df"><button class="btn primary">Done</button></div>
</div>`;
  const behind = `${memberHeader("self")}<section class="section"><div class="sh"><div><h2>Personal Postgres logins</h2></div></div>${loginsTable("self")}</section>`;
  return dialogOver("12-login-reveal.html", "Personal login password", `<span>Members</span>${I("right", "s14")}<b>Sofia Marchetti</b>`, behind, dlg, "The shown-once reveal after New login (or Reset / Renew): password and main-branch connection string, each with Copy; P23's note on branches; the roles the login can SET ROLE into. 'You won't see this password again' is plain chalk weight, not a warning colour. Done is the only way out (no click-outside dismissal); the secret lives in the dialog's state only.", "Sofia Marchetti");
}
function inviteCompose() {
  const dlg = `<div class="dialog wide" role="dialog" aria-modal="true" aria-labelledby="iv-t">
  <div><div class="dt" id="iv-t">Invite people</div></div>
  <div class="fl"><span class="lbl">Emails</span>
    <div class="chips-field field focus" style="height:auto">
      <span class="chip">rui.costa@northwind.dev<span class="x">${I("x", "s12")}</span></span>
      <span class="chip">nadia.petrova@northwind.dev<span class="x">${I("x", "s12")}</span></span>
      <span class="chip bad" title="Not an email address">ben@northwind<span class="x">${I("x", "s12")}</span></span>
      <span class="chip">ines.duarte@northwind.dev<span class="x">${I("x", "s12")}</span></span>
      <span class="chip">ops-oncall@contractor.io<span class="x">${I("x", "s12")}</span></span>
      <span class="mono dim" style="font-size:12px;align-self:center;padding:0 4px">|</span>
    </div>
    <span class="help">Paste or type addresses, separated by commas, spaces or new lines. Up to 50.</span>
    <span class="note coral">ben@northwind isn't an email address. Fix it or remove it; the others can go.</span></div>
  <div class="fl"><span class="lbl">How they get the invite</span>
    <div style="display:flex;gap:24px;flex-wrap:wrap"><label style="display:flex;gap:8px;align-items:center"><span class="radio on"></span>Email <span class="dim note">We email each a link.</span></label><label style="display:flex;gap:8px;align-items:center"><span class="radio"></span>One-time links <span class="dim note">You send them yourself.</span></label></div></div>
  <div class="fl"><span class="lbl">Access <span class="dim" style="font-weight:400">· the same for everyone in this invite</span></span>
    <div class="grow"><span class="field select"><span class="v">Billing app</span>${I("down", "s16 dim")}</span>${segRole("Developer", false, ["Viewer", "Developer", "Admin"])}<span class="field" style="height:auto;min-height:36px;flex-wrap:wrap;padding:4px"><span class="chip">billing_reader</span><span class="chip">support_agent</span></span><button class="btn icon sm ghost" aria-label="Remove Billing app">${I("x", "s16")}</button></div>
    <button class="btn ghost sm" style="justify-self:start">${I("plus", "s16")}Add a project</button></div>
  <div class="df"><span class="note dim" style="margin-right:auto;align-self:center">4 invites · 1 address to fix</span><button class="btn ghost">Cancel</button><button class="btn primary">Send 4 invites</button></div>
</div>`;
  const behind = `${membersHead()}${toolbar({ count: "14 people", view: "cards", admin: true })}`;
  return dialogOver("13-invite-compose.html", "Invite people", "<b>Members</b>", behind, dlg, "Several emails per invite: pasted addresses become chips, an invalid one is coral-outlined with its reason and is never sent (the button counts only valid ones). Delivery and access are shared by the batch; access rows are today's grant rows.");
}
function inviteResults() {
  const dlg = `<div class="dialog wide" role="dialog" aria-modal="true" aria-labelledby="ir-t">
  <div><div class="dt" id="ir-t">Invites sent: 3 of 4</div><p class="dd">Developer on Billing app with billing_reader, support_agent.</p></div>
  <div class="results" role="list">
    <div class="rr" role="listitem"><span class="sq sea"></span><div><span class="e">rui.costa@northwind.dev</span><p class="note dim">Invited. The link in their email works for 72 hours.</p></div></div>
    <div class="rr" role="listitem"><span class="sq sea"></span><div><span class="e">nadia.petrova@northwind.dev</span><p class="note dim">Invited again with a new link. The earlier one stopped working.</p></div></div>
    <div class="rr" role="listitem"><span class="sq sea"></span><div><span class="e">ines.duarte@northwind.dev</span><p class="note dim">Already a member. Her access on Billing app is set.</p></div></div>
    <div class="rr" role="listitem"><span class="sq coral"></span><div><span class="e">ops-oncall@contractor.io</span><p class="note"><span class="coral">Not sent:</span> another account outside this organisation already uses this email. Use a different email, or fix the account in the identity provider.</p></div></div>
  </div>
  <p class="note dim">With one-time links, the links are listed here together, each shown once, with Copy.</p>
  <div class="df"><button class="btn ghost">Edit and retry the failed one</button><button class="btn primary">Done</button></div>
</div>`;
  const behind = `${membersHead()}${toolbar({ count: "16 people", view: "cards", admin: true })}`;
  return dialogOver("14-invite-results.html", "Invite results", "<b>Members</b>", behind, dlg, "After sending: one result per email (R8: sent in sequence), seafoam square for done, solid coral for failed with the server's reason in words. The title counts. Failed addresses can go back into the form; the successful ones aren't sent twice.");
}
function suspendDialog() {
  const dlg = `<div class="dialog" role="alertdialog" aria-modal="true" aria-labelledby="sd-t">
  <div><div class="dt" id="sd-t">Suspend Sofia Marchetti?</div></div>
  <p>Sofia is signed out of Studio and can't sign in. Her personal Postgres logins stop working at once and every database session she has open ends.</p>
  <p class="dim note">Her project access and roles are kept, so Reinstate gives them back. She makes new personal logins herself afterwards. To take everything away for good, remove her from the tenant instead.</p>
  <div class="df"><button class="btn ghost">Cancel</button><button class="btn danger">Suspend Sofia</button></div>
</div>`;
  const behind = memberHeader("admin");
  return dialogOver("15-suspend-dialog.html", "Suspend member", `<span>Members</span>${I("right", "s14")}<b>Sofia Marchetti</b>`, behind, dlg, "Suspend's confirm: what stops now, what's kept, how to undo it, and the permanent alternative. Destructive button, no sand.");
}
function revokeDialog() {
  const dlg = `<div class="dialog" role="alertdialog" aria-modal="true" aria-labelledby="rv-t">
  <div><div class="dt" id="rv-t">Revoke Sofia's Postgres access now?</div></div>
  <p>For a password that may have leaked. At once:</p>
  <ul class="note" style="margin:0;padding-left:18px;display:grid;gap:4px">
    <li>her personal logins stop working: <span class="mono">sofia</span> on billing-prod, <span class="mono">sofia_m</span> on warehouse;</li>
    <li>every Postgres session she has open ends, including her SQL editor and table editor sessions, which reconnect with new passwords.</li>
  </ul>
  <p class="dim note">Her Studio access and project roles don't change, and she can create new personal logins.</p>
  <div class="df"><button class="btn ghost">Cancel</button><button class="btn danger">Revoke access now</button></div>
</div>`;
  const behind = memberHeader("admin");
  return dialogOver("16-revoke-now-dialog.html", "Revoke Postgres access", `<span>Members</span>${I("right", "s14")}<b>Sofia Marchetti</b>`, behind, dlg, "Revoke Postgres access now: names the logins that stop, says sessions end (P16: cb_m_* passwords rotate, so Studio's own sessions reconnect), and what's untouched.");
}

// ---------------------------------------------------------------- build
const comps = [
  rosterCards(),
  rosterListDev(),
  rosterProject(),
  memberPage("admin"),
  memberPage("self"),
  memberSuspended(),
  rolesTab(),
  rolePage(),
  roleReview("review"),
  roleReview("conflict"),
  createRole(),
  loginReveal(),
  inviteCompose(),
  inviteResults(),
  suspendDialog(),
  revokeDialog(),
];
const esc = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;");
const index = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Members hub comps</title>
<link href="https://fonts.googleapis.com/css2?family=Atkinson+Hyperlegible+Next:wght@400;500;600&family=JetBrains+Mono:wght@400;500&display=swap" rel="stylesheet">
<style>
:root { --bg: #f4f1ec; --fg: #1d1a17; --dim: #5f574f; --line: rgb(29 26 23 / 0.15); --accent: #8a5a32; color-scheme: light; }
@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) { --bg: #0c0b0a; --fg: #ebe6e0; --dim: #b0a89f; --line: rgb(235 230 224 / 0.15); --accent: #f2c7a3; color-scheme: dark; } }
:root[data-theme="dark"] { --bg: #0c0b0a; --fg: #ebe6e0; --dim: #b0a89f; --line: rgb(235 230 224 / 0.15); --accent: #f2c7a3; color-scheme: dark; }
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--fg); font: 15px/1.5 "Atkinson Hyperlegible Next", system-ui, sans-serif; }
main { max-width: 72rem; margin: 0 auto; padding: 40px 16px 64px; }
h1 { font-size: 28px; margin: 0 0 6px; }
h2 { font-size: 18px; margin: 40px 0 12px; }
p, li { max-width: 72ch; }
.dim { color: var(--dim); }
code { font: 13px "JetBrains Mono", ui-monospace, monospace; }
a { color: inherit; text-underline-offset: 3px; }
a:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
ol.comps { list-style: none; padding: 0; margin: 0; display: grid; grid-template-columns: repeat(auto-fill, minmax(320px, 1fr)); gap: 20px; }
ol.comps li { border: 1px solid var(--line); padding: 12px; display: grid; gap: 8px; align-content: start; max-width: none; }
ol.comps img { width: 100%; height: 200px; object-fit: cover; object-position: top left; border: 1px solid var(--line); display: block; }
ol.comps h3 { margin: 0; font-size: 15px; }
ol.comps .links { font-size: 13px; display: flex; gap: 12px; flex-wrap: wrap; }
ol.comps p { margin: 0; font-size: 14px; color: var(--dim); }
</style>
</head>
<body><main>
<h1>Members hub, step 1: comps</h1>
<p class="dim">Static comps in Studio's Basalt system (dark, as Studio ships). Each opens as a page; PNGs are full-page captures at 1440 and 390 wide. Realistic data: Northwind Labs, 14 people across Billing app, Ops console and Data warehouse.</p>
<h2>Look at these first</h2>
<ul>
<li><b>Roster cards</b> (01): square cards, initials in a hairline square, level as a tag; every group carries a one-line meaning. Cards and list share the groups.</li>
<li><b>Grant card</b> (04): segmented platform role, the role checklist (checked first, "also gives", Admin-only with reasons, escaped names), one Save per card, unsaved state as a sand edge.</li>
<li><b>Privilege editor</b> (08–10): one matrix per schema with a column drill-in, staged cells in sand, greyed "owned by X" rows, and the review / refusal / conflict states in the one right panel.</li>
<li><b>Personal logins</b> (04, 05, 12): Admins see Delete only; Renew lives on the owner's own page; the reveal says where the login works (P23).</li>
<li><b>Phone width</b>: the rail becomes a header menu trigger (DESIGN.md's open gap); matrices scroll inside their frame, the page never scrolls sideways.</li>
</ul>
<h2>Screens</h2>
<ol class="comps">
${comps
  .map((c) => {
    const base = c.file.replace(/\.html$/, "");
    return `<li><a href="${c.file}"><img src="png/${base}-1440.png" alt="${esc(c.title)} at 1440 wide" loading="lazy"></a>
<h3><a href="${c.file}">${base.slice(0, 2)} · ${esc(c.title)}</a></h3>
<div class="links"><a href="png/${base}-1440.png">PNG 1440</a><a href="png/${base}-390.png">PNG 390</a></div>
<p>${esc(c.note)}</p></li>`;
  })
  .join("\n")}
</ol>
</main></body></html>
`;
writeFileSync(join(OUT, "index.html"), index);
console.log(`${comps.length} comps written`);
