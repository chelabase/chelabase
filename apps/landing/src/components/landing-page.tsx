import type { ReactNode } from "react";
import { Archive, Braces, Database, DoorOpen, GitBranch, LayoutDashboard, SquareTerminal, Users, type LucideIcon } from "lucide-react";
import { ClawMark, CrabFigure, Wordmark } from "@chelabase/lagoon/crab-marks";
import { ShoreScene } from "./shore-scene";
import { FOCUS, INSET_LABEL, TideGauge } from "./tide-gauge";

export interface LandingPageProps {
  /** Studio's origin, no trailing slash: "Start free" and "Log in" link into it. */
  studioUrl: string;
}

/* ---------- Shared pieces ---------- */

/** The page's centred column, gutters included. */
const COLUMN = "w-full max-w-[calc(var(--container-landing)+2*var(--spacing-gutter))] mx-auto px-gutter";

const SECTION = "relative px-gutter pt-section";
const SECTION_INNER = "max-w-landing mx-auto";
const SECTION_TITLE = "mb-5 font-serif font-semibold text-[2.5rem] leading-[1.05] tracking-tight text-balance";
const SECTION_LEDE = "max-w-[62ch] text-[1.125rem] text-pretty";
/** Two columns, stacked below 900px. */
const SPLIT = "grid grid-cols-[minmax(0,1fr)_minmax(0,1.05fr)] gap-18 items-start max-[900px]:grid-cols-1";

/** Inline code in running text; the colour is set by the ground it sits on. */
const CODE = "[&_code]:font-mono [&_code]:text-[0.88em]";
const CODE_ON_INK = `${CODE} [&_code]:text-sand`;

const INSET = "m-0 border border-line-strong rounded-xl overflow-hidden bg-ink text-chalk shadow-[0_24px_50px_-30px] shadow-black/60";
const INSET_PRE = `m-0 px-4 pt-4.5 pb-5 overflow-x-auto font-mono text-[0.86rem] leading-[1.7] text-chalk ${FOCUS}`;

type Ground = "ink" | "sand";

/** A plate (button-shaped link). `ground` is the colour it sits on: sand plates turn to ink on the sand band. */
function plate({ primary = false, small = false, ground = "ink" }: { primary?: boolean; small?: boolean; ground?: Ground }) {
  const base =
    "inline-flex items-center justify-center gap-[0.5em] border-[1.5px] font-sans font-semibold leading-none cursor-pointer " +
    "transition-[background-color,box-shadow,translate] duration-[180ms,220ms,220ms] ease-lagoon " +
    "hover:-translate-y-px active:translate-y-0 focus-visible:outline-2 focus-visible:outline-offset-3";
  const size = small ? "min-h-8 px-3 rounded-md text-sm" : "min-h-9.5 px-4 rounded-lg text-[0.9375rem]";
  const glow = "hover:shadow-[0_8px_20px_-8px] hover:shadow-sand/50";
  const tone =
    ground === "ink"
      ? primary
        ? `border-sand bg-sand text-ink hover:bg-sand-lit ${glow} focus-visible:outline-sand`
        : "border-chalk bg-transparent text-chalk hover:bg-chalk/8 focus-visible:outline-sand"
      : primary
        ? `border-ink bg-ink text-sand hover:bg-ink-lit ${glow} focus-visible:outline-ink`
        : "border-ink bg-transparent text-ink hover:bg-ink/8 focus-visible:outline-ink";
  return `${base} ${size} ${tone}`;
}

function Actions({ studioUrl, ground = "ink", className = "mt-6.5" }: LandingPageProps & { ground?: Ground; className?: string }) {
  // Below 560px each plate takes a full row.
  const full = "max-[560px]:grow max-[560px]:basis-full";
  return (
    <div className={`flex flex-wrap gap-3 ${className}`}>
      <a className={`${plate({ primary: true, ground })} ${full}`} href={`${studioUrl}/?signup`}>
        Start free
      </a>
      <a className={`${plate({ ground })} ${full}`} href={`${studioUrl}/`}>
        Log in
      </a>
    </div>
  );
}

/** A band's top edge: a wave in the band's own colour, rising out of the section above. */
function Wave({ d, className }: { d: string; className: string }) {
  return (
    <svg
      className={`absolute inset-x-0 top-0 block w-full h-7 -translate-y-[27px] ${className}`}
      viewBox="0 0 1200 28"
      preserveAspectRatio="none"
      aria-hidden="true"
    >
      <path d={d} />
    </svg>
  );
}

const BAND_WAVE = "M0 28V18C120 6 220 4 340 12s240 16 380 6 300-18 480-8v18Z";
const LANDFALL_WAVE = "M0 28V14C160 4 300 2 460 10s300 18 440 8 220-14 300-10v20Z";

/** The claw and its eyes carry the mark's weight high; settle it onto the row's centre line. */
const SETTLE = "translate-y-0.5";
/** Landing-only nudges of the wordmark's claw: it sits a little low and may overflow its box. */
const CLAW = "-mb-[0.1em] overflow-visible";

export function LandingPage({ studioUrl }: LandingPageProps) {
  return (
    <div className="lagoon min-h-screen bg-ink text-chalk font-sans leading-[1.6] antialiased overflow-x-clip">
      <nav className="sticky top-0 z-20 h-16 bg-ink border-b border-line" aria-label="Chelabase">
        <div className={`${COLUMN} flex items-center justify-between gap-4 h-full`}>
          <a href="/" className={`flex items-center text-[1.25rem] ${FOCUS}`}>
            <Wordmark visualClassName={SETTLE} clawClassName={`h-[1.32em] ${CLAW}`} />
          </a>
          <div className="flex gap-2.5">
            <a className={plate({ primary: true, small: true })} href={`${studioUrl}/?signup`}>
              Start free
            </a>
            <a className={`${plate({ small: true })} max-[560px]:hidden`} href={`${studioUrl}/`}>
              Log in
            </a>
          </div>
        </div>
      </nav>
      {/* Below the sticky bar, the hero fills the rest of the screen up to a cap, so the whole shore is in
          the first view; everything inside it is fixed-size and sits in the centred column. */}
      <header className="relative flex flex-col h-[clamp(560px,calc(100svh-64px),820px)] bg-ink max-[560px]:h-auto">
        <div className={`${COLUMN} relative z-1 flex-none pt-12 short:pt-7`}>
          <Wordmark as="h1" className="m-0 text-[4rem] short:text-[3.25rem]" clawClassName={`h-[1.3em] ${CLAW}`} />
          <p className="mt-4.5 font-serif font-semibold text-[1.75rem] leading-[1.2] tracking-[-0.015em] short:mt-3 short:text-[1.5rem]">
            Postgres that branches. Run it yourself.
          </p>
          <p className="mt-2.5 max-w-160 text-chalk-dim text-[1.125rem] text-pretty">
            Postgres on Neon's storage engine, with copy-on-write branches, point-in-time restore, a typed data API and
            auth for your app's users, managed from one console. Self-hosted today.
          </p>
          <Actions studioUrl={studioUrl} className="mt-6 short:mt-4.5" />
        </div>
        <ShoreScene />
      </header>

      <main>
        <WhatItIs />
        <Setup />
        <Branching />
        <Pieces />
        <Compare />

        <section
          className="relative mt-section px-gutter py-section bg-sand text-ink [--claw-gap:var(--sand)]"
          aria-labelledby="landfall-title"
        >
          <Wave d={LANDFALL_WAVE} className="fill-sand" />
          <div className="max-w-250 mx-auto grid grid-cols-[minmax(160px,0.7fr)_minmax(0,1.3fr)] gap-14 items-center max-[900px]:grid-cols-1 max-[900px]:justify-items-start">
            <svg className="w-full h-auto max-w-80 max-[900px]:max-w-50" viewBox="0 0 240 190" aria-hidden="true">
              <CrabFigure />
            </svg>
            <div>
              <h2 id="landfall-title" className="font-serif font-semibold text-[2.75rem] leading-[1.05] tracking-tight">
                Get your first branch.
              </h2>
              <p className={`mt-3.5 text-ink/82 text-[1.125rem] ${CODE} [&_code]:text-shell-dark [&_code]:font-semibold`}>
                Sign up, create a database, branch it. For the docs, run <code>just docs</code> in the repo.
              </p>
              <Actions studioUrl={studioUrl} ground="sand" />
            </div>
          </div>
        </section>
      </main>

      <footer className={`${COLUMN} flex flex-wrap items-center justify-between gap-x-8 gap-y-4 pt-5.5 pb-7 text-chalk-dim text-[0.92rem]`}>
        <div className="flex items-center gap-2">
          <Wordmark className="text-[1.15rem]" visualClassName={SETTLE} clawClassName={`h-[1.32em] ${CLAW}`} />
          <span>· self-hosted preview</span>
        </div>
        <span>Platform mode isn't built yet. Drawings on this page are illustrations.</span>
      </footer>
    </div>
  );
}

/* ---------- What it is ---------- */

const NOTES: Array<{ title: string; body: ReactNode }> = [
  {
    title: "Safe by default",
    body: "Credentials are encrypted at rest and revealable when you need them. Secrets stay out of client caches, and Studio flags tables the data API can reach without row-level security.",
  },
  {
    title: "A typed data API",
    body: (
      <>
        One endpoint, <code>POST /data/v1/query</code>, with a builder client and types generated from your schema. No
        URL grammar to learn.
      </>
    ),
  },
  {
    title: "Postgres stays Postgres",
    body: "Branches, login roles and policies show up as what they are. Connect with psql whenever you like.",
  },
];

function WhatItIs() {
  return (
    // The first band meets the hero's beach directly: no top margin.
    <section
      className={`${SECTION} pb-section bg-seafoam text-ink [--claw-gap:var(--seafoam)] selection:bg-ink selection:text-seafoam`}
      aria-labelledby="what-title"
    >
      <Wave d={BAND_WAVE} className="fill-seafoam" />
      <div className={`${SECTION_INNER} ${SPLIT}`}>
        <div>
          <h2 id="what-title" className={SECTION_TITLE}>
            A database platform with nothing hidden.
          </h2>
          <p className={`${SECTION_LEDE} text-ink/80`}>
            Each database gets its own tenant, timeline and compute, plus branches, login roles, a data API and auth for
            your app's users. Studio shows all of it, including what isn't secured yet.
          </p>
          <ul className="mt-9 grid gap-6.5">
            {NOTES.map((n) => (
              <li key={n.title} className="grid grid-cols-[38px_minmax(0,1fr)] gap-x-3.5 gap-y-1">
                <ClawMark size={30} className="row-span-2 mt-0.5" />
                <h3 className="font-serif font-semibold text-[1.3rem] leading-[1.3]">{n.title}</h3>
                <p className={`max-w-[52ch] text-ink/80 ${CODE} [&_code]:text-ink [&_code]:font-semibold`}>{n.body}</p>
              </li>
            ))}
          </ul>
        </div>
        <figure className={INSET}>
          <figcaption className={`${INSET_LABEL} ${CODE_ON_INK}`}>
            <span>Your app, with @chelabase/client</span>
            <code>orders.ts</code>
          </figcaption>
          <pre className={INSET_PRE}>
            <code>
              <span className="text-coral">import</span> {"{ "}
              <span className="text-seafoam">createClient</span>
              {" }"} <span className="text-coral">from</span> <span className="text-sand">"@chelabase/client"</span>;{"\n"}
              <span className="text-coral">import</span> {"{ "}
              <span className="text-coral">type</span> <span className="text-palm">Database</span>, types {"}"}{" "}
              <span className="text-coral">from</span> <span className="text-sand">"./db"</span>;{"\n"}
              <span className="text-chalk-dim italic">// ./db is written by `chelabase gen types`</span>
              {"\n\n"}
              <span className="text-coral">const</span> db = <span className="text-seafoam">createClient</span>&lt;
              <span className="text-palm">Database</span>&gt;({"{ "}url, key, types{" }"});{"\n\n"}
              <span className="text-coral">const</span> paid = <span className="text-coral">await</span> db{"\n"}
              {"  "}.<span className="text-seafoam">from</span>(<span className="text-sand">"orders"</span>){"\n"}
              {"  "}.<span className="text-seafoam">select</span>(<span className="text-sand">"id"</span>,{" "}
              <span className="text-sand">"total"</span>){"\n"}
              {"  "}.<span className="text-seafoam">where</span>({"{ "}status: <span className="text-sand">"paid"</span>
              {" }"});{"\n"}
              <span className="text-chalk-dim italic">// paid: {"{ id: bigint; total: string }"}[]</span>
            </code>
          </pre>
        </figure>
      </div>
    </section>
  );
}

/* ---------- From clone to a query ---------- */

const STEPS: Array<{ title: string; body: ReactNode }> = [
  {
    title: "Clone and start",
    body: (
      <>
        <code>just dev</code> starts the dependencies, sets up sign-in, and runs the services and Studio. You need proto,
        Docker and a Rust toolchain; the first run builds a lot.
      </>
    ),
  },
  { title: "Create an account", body: "Open Studio and sign up. Email verification is off in self-hosted mode." },
  { title: "Create a database", body: "Name it. Chelabase makes its tenant, timeline and compute, and Studio shows it as ready." },
  { title: "Branch it", body: "Copy main as it is now, or from any point in its history window, to try a change safely." },
  { title: "Query it", body: "From Studio's SQL editor, or from your app with the typed client." },
];

function Setup() {
  return (
    <section className={SECTION} aria-labelledby="setup-title">
      <div className={`${SECTION_INNER} ${SPLIT}`}>
        <div>
          <h2 id="setup-title" className={SECTION_TITLE}>
            From clone to a query.
          </h2>
          {/* a dotted line threads the step numbers */}
          <ol className="relative mt-8 grid gap-6.5 before:absolute before:left-4.5 before:top-5 before:bottom-5 before:border-l-2 before:border-dotted before:border-shell before:opacity-70">
            {STEPS.map((s, i) => (
              <li key={s.title} className="relative grid grid-cols-[38px_minmax(0,1fr)] gap-x-4.5 gap-y-0.5">
                <span
                  className="row-span-2 size-9.5 grid place-items-center border-2 border-shell rounded-full bg-ink font-serif font-semibold text-shell"
                  aria-hidden="true"
                >
                  {i + 1}
                </span>
                <h3 className="mt-1.25 font-serif font-semibold text-[1.3rem]">{s.title}</h3>
                <p className={`max-w-[52ch] text-chalk-dim ${CODE_ON_INK}`}>{s.body}</p>
              </li>
            ))}
          </ol>
        </div>
        <figure className={`${INSET} sticky top-6 max-[900px]:static`} aria-label="Your terminal: setup from the README">
          <figcaption className={INSET_LABEL}>
            <span>Your terminal</span>
            <span>from the README</span>
          </figcaption>
          <pre className={INSET_PRE}>
            <code>
              <span className="text-seafoam">git</span> clone https://github.com/chelabase/chelabase.git{"\n"}
              <span className="text-seafoam">cd</span> chelabase{"\n"}
              <span className="text-seafoam">proto</span> install{"\n"}
              <span className="text-seafoam">pnpm</span> install{"\n"}
              <span className="text-seafoam">just</span> dev{"\n\n"}
              <span className="text-chalk-dim italic"># then open http://localhost:3000</span>
              {"\n"}
              <span className="text-chalk-dim italic"># stop everything with: just stop</span>
            </code>
          </pre>
        </figure>
      </div>
    </section>
  );
}

/* ---------- Branching ---------- */

const TERM = "font-serif font-semibold text-[1.25rem]";
const DEFINITION = "mt-1 max-w-[52ch] text-chalk-dim";

function Branching() {
  return (
    <section className={`${SECTION} mt-section pb-section bg-shallows`} aria-labelledby="branch-title">
      <Wave d={BAND_WAVE} className="fill-shallows" />
      <div
        className={`${SECTION_INNER} grid grid-cols-[minmax(0,0.9fr)_minmax(0,1.4fr)] gap-16 items-start max-[900px]:grid-cols-1`}
      >
        <div>
          <h2 id="branch-title" className={SECTION_TITLE}>
            Branch it. Reset it. Restore it.
          </h2>
          <p className={`${SECTION_LEDE} text-chalk-dim`}>
            Every database starts with one branch, main. Branches form a tree, and each one has its own timeline,
            endpoint and password.
          </p>
          <dl className="mt-8 grid gap-5">
            <div>
              <dt className={TERM}>Branch</dt>
              <dd className={DEFINITION}>
                Copy any branch as it is now, or as it was at any point inside its history window (seven days by
                default). The copy is copy-on-write, so it's ready without duplicating the data.
              </dd>
            </div>
            <div>
              <dt className={TERM}>Reset</dt>
              <dd className={DEFINITION}>Bring a branch back in line with its parent's latest state, keeping its connection string.</dd>
            </div>
            <div>
              <dt className={TERM}>Restore</dt>
              <dd className={DEFINITION}>
                Take a branch back to a point in time. The old timeline is kept as a backup branch, in case you want it
                back.
              </dd>
            </div>
          </dl>
        </div>
        <TideGauge />
      </div>
    </section>
  );
}

/* ---------- Product pieces ---------- */

interface Piece {
  id: string;
  icon: LucideIcon;
  title: string;
  body: ReactNode;
  soon?: boolean;
  secondary?: boolean;
}

const PIECES: Piece[] = [
  {
    id: "database",
    icon: Database,
    title: "Database",
    body: "Postgres on Neon's storage engine. Each database gets its own tenant, timeline and compute.",
  },
  {
    id: "branches",
    icon: GitBranch,
    title: "Branches",
    body: "Copy-on-write branches with reset, point-in-time restore and protected branches.",
  },
  {
    id: "data-api",
    icon: Braces,
    title: "Data API and typed client",
    body: (
      <>
        <code>POST /data/v1/query</code> behind <code>anon</code> and <code>authenticated</code> roles and row-level
        security, with <code>@chelabase/client</code> and generated types.
      </>
    ),
  },
  {
    id: "app-users",
    icon: Users,
    title: "App users",
    body: "Sign-up, sign-in and sessions for your app's users, wired to row-level security.",
  },
  {
    id: "studio",
    icon: LayoutDashboard,
    title: "Studio",
    body: "Projects, databases, the branch tree, login roles, tables, a SQL editor, data API keys and your app's users.",
  },
  {
    id: "cli",
    icon: SquareTerminal,
    title: "CLI",
    body: (
      <>
        <code>chelabase</code>, run from the repo: <code>dev bootstrap</code> writes the local stack's keys and config, and{" "}
        <code>gen types</code> generates types from a branch's schema.
      </>
    ),
  },
  {
    id: "storage",
    icon: Archive,
    title: "Object storage",
    body: "Files for your app, planned on SeaweedFS, which already holds Neon's storage in the stack. Not available yet.",
    soon: true,
  },
  {
    id: "gateway",
    icon: DoorOpen,
    title: "Gateway",
    body: "One front door for every API: management, data and auth.",
    secondary: true,
  },
];

function Pieces() {
  return (
    <section className={SECTION} aria-labelledby="pieces-title">
      <div className={SECTION_INNER}>
        <h2 id="pieces-title" className={SECTION_TITLE}>
          Everything in one place.
        </h2>
        <p className={`${SECTION_LEDE} text-chalk-dim`}>
          The pieces fit because they were built together: one gateway, one console, one set of roles.
        </p>
        <ul className="mt-11 grid grid-cols-2 gap-x-14 border-t border-line-strong max-[900px]:grid-cols-1">
          {PIECES.map((p) => (
            <li
              key={p.id}
              className="grid grid-cols-[44px_minmax(0,1fr)] gap-x-4 gap-y-1 py-5.5 border-b border-line"
              aria-labelledby={`piece-${p.id}`}
            >
              <span
                className={`row-span-2 size-11 grid place-items-center rounded-xl ${
                  p.soon ? "border-[1.5px] border-dashed border-coral text-coral" : "bg-seafoam/12 text-seafoam"
                }`}
                aria-hidden="true"
              >
                <p.icon size={22} strokeWidth={1.6} />
              </span>
              <h3
                id={`piece-${p.id}`}
                className={`flex flex-wrap items-baseline gap-x-3 gap-y-1 mt-2 font-serif font-semibold leading-[1.3] ${
                  p.secondary ? "text-[1.1rem]" : "text-[1.25rem]"
                }`}
              >
                {p.title}
                {p.soon && (
                  <span className="inline-block px-2.25 py-px border-[1.5px] border-dashed border-coral rounded-full font-sans text-[0.8rem] font-semibold text-coral whitespace-nowrap">
                    Coming soon
                  </span>
                )}
              </h3>
              <p className={`max-w-[54ch] text-chalk-dim ${CODE_ON_INK}`}>{p.body}</p>
            </li>
          ))}
        </ul>
      </div>
    </section>
  );
}

/* ---------- Compare ---------- */

type Mark = "yes" | "partial" | "soon" | "no" | "other" | "unknown";

const MARK_TEXT: Record<Mark, string> = {
  yes: "Yes",
  partial: "Partly",
  soon: "Coming soon",
  no: "No",
  other: "Different model",
  unknown: "To survey",
};

/** The mark's colour; its glyph (`.mark--*::before`) is drawn in landing.css. */
const MARK_CLASS: Record<Mark, string> = {
  yes: "text-seafoam",
  partial: "text-sand",
  soon: "text-coral",
  no: "text-chalk-dim",
  other: "text-chalk-dim",
  unknown: "text-chalk-dim italic font-serif",
};

const CELL = "p-3.5 text-left";
const HEAD = "pb-3 border-b-2 border-line-strong font-serif font-semibold text-[1.1rem]";

const COLUMNS = ["Chelabase", "Supabase", "Firebase", "Vanilla Neon", "PlanetScale"] as const;

const ROWS: Array<{ label: string; marks: [Mark, Mark, Mark, Mark, Mark] }> = [
  { label: "Postgres", marks: ["yes", "yes", "other", "yes", "unknown"] },
  { label: "Copy-on-write branches", marks: ["yes", "other", "no", "yes", "unknown"] },
  { label: "Point-in-time restore", marks: ["yes", "yes", "unknown", "yes", "unknown"] },
  { label: "Run it yourself", marks: ["yes", "yes", "no", "partial", "unknown"] },
  { label: "Typed query API", marks: ["yes", "other", "other", "unknown", "unknown"] },
  { label: "App-user auth", marks: ["yes", "yes", "yes", "unknown", "unknown"] },
  { label: "Object storage", marks: ["soon", "yes", "yes", "no", "unknown"] },
  { label: "Lock-in posture", marks: ["unknown", "unknown", "unknown", "unknown", "unknown"] },
];

function Compare() {
  return (
    <section className={SECTION} aria-labelledby="compare-title">
      <div className={SECTION_INNER}>
        <h2 id="compare-title" className={SECTION_TITLE}>
          How it compares, roughly.
        </h2>
        <p className={`${SECTION_LEDE} text-chalk-dim`}>
          A first sketch of the comparison we plan to publish. It's a wireframe: nothing here is a measurement.
        </p>
        {/* a wireframe, not a result */}
        <div className="mt-10 p-7 border-2 border-dashed border-line-strong rounded-2xl">
          <p className="inline-block mb-5.5 px-3.5 py-1.5 border-2 border-coral rounded-md text-coral font-serif font-bold text-[0.95rem] -rotate-[1.5deg]">
            Comparison preview — benchmarks coming later
          </p>
          <div className={`overflow-x-auto ${FOCUS}`}>
            <table className="w-full min-w-170 border-collapse text-[0.97rem]" aria-label="Comparison preview">
              <thead>
                <tr>
                  <td className={`${CELL} align-middle`} />
                  {COLUMNS.map((c) => (
                    <th key={c} scope="col" className={`${CELL} ${HEAD} align-bottom`}>
                      {/* Chelabase's own column is set in the wordmark, which is taller; the names align on the bottom. */}
                      {c === "Chelabase" ? <Wordmark visualClassName="translate-y-[0.24em]" clawClassName={`h-[1.32em] ${CLAW}`} /> : c}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {ROWS.map((r, row) => {
                  const rule = `align-middle ${row > 0 ? "border-t border-line" : ""}`;
                  return (
                    <tr key={r.label}>
                      <th scope="row" className={`${CELL} ${rule} w-[24%] font-semibold text-chalk`}>
                        {r.label}
                      </th>
                      {r.marks.map((m, i) => (
                        <td key={COLUMNS[i]} className={`${CELL} ${rule} ${i === 0 ? "bg-sand/5" : ""}`}>
                          <span className={`mark mark--${m} flex items-center gap-2 ${MARK_CLASS[m]}`}>{MARK_TEXT[m]}</span>
                        </td>
                      ))}
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
          <p className="mt-4.5 max-w-[70ch] text-chalk-dim text-[0.95rem]">
            "To survey" means we haven't checked it yet. Chelabase doesn't aim to be compatible with Supabase or Firebase;
            it's its own design on Neon's storage engine.
          </p>
        </div>
      </div>
    </section>
  );
}
