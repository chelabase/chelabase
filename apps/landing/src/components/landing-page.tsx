import { createContext, useContext, type ReactNode } from "react";
import {
  Archive,
  ArrowUpRight,
  Braces,
  Cog,
  Database,
  DoorOpen,
  KeyRound,
  LayoutDashboard,
  Radio,
  SquareTerminal,
  Waypoints,
  type LucideIcon,
} from "lucide-react";
import { ClawMark, CrabFigure, Wordmark } from "@chelabase/design/crab-marks";
import { docsHref, GITHUB_URL } from "@/site-urls";
import { ShoreScene } from "./shore-scene";
import { FOCUS, INSET_LABEL, TideGauge } from "./tide-gauge";

export interface LandingPageProps {
  /** The docs site's origin (build-time `VITE_DOCS_URL`); without it, docs links open the pages on GitHub. */
  docsUrl?: string;
}

/* ---------- Shared pieces ---------- */

/** The page's centred column, gutters included. */
const COLUMN = "w-full max-w-[calc(var(--container-landing)+2*var(--spacing-gutter))] mx-auto px-gutter";

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

/** A plate (button-shaped link). */
function plate({ primary = false, small = false }: { primary?: boolean; small?: boolean } = {}) {
  const base =
    "inline-flex items-center justify-center gap-[0.5em] border-[1.5px] font-sans font-semibold leading-none cursor-pointer " +
    "transition-[background-color,box-shadow,translate] duration-[180ms,220ms,220ms] ease-design " +
    "hover:-translate-y-px active:translate-y-0 focus-visible:outline-2 focus-visible:outline-offset-3 focus-visible:outline-sand";
  const size = small ? "min-h-8 px-3 rounded-md text-sm" : "min-h-9.5 px-4 rounded-lg text-[0.9375rem]";
  const tone = primary
    ? "border-sand bg-sand text-ink hover:bg-sand-lit hover:shadow-[0_8px_20px_-8px] hover:shadow-sand/50"
    : "border-chalk bg-transparent text-chalk hover:bg-chalk/8";
  return `${base} ${size} ${tone}`;
}

function Actions({ docsUrl, className = "mt-6.5" }: { docsUrl?: string; className?: string }) {
  // Below 560px each plate takes a full row.
  const full = "max-[560px]:grow max-[560px]:basis-full";
  return (
    <div className={`flex flex-wrap gap-3 ${className}`}>
      <a className={`${plate({ primary: true })} ${full}`} href={docsHref("using/quickstart", docsUrl)}>
        Get started
      </a>
      <a className={`${plate()} ${full}`} href={GITHUB_URL}>
        Visit GitHub
      </a>
    </div>
  );
}

type Tone = "raised" | "ink";

/**
 * Bands below the hero alternate raised (deep) and ink, starting raised; `flip` mirrors every other edge so they
 * don't repeat. The comparison sits in an ink panel whichever band it lands in.
 */
const BandTone = createContext<{ tone: Tone; flip: boolean }>({ tone: "raised", flip: false });

/** A band's top edge: a low swell in the band's own colour, with a sand tideline along its crest. */
const CREST = "M0 12C150 4 330 2 520 8s390 12 680 2";

function Edge() {
  const { flip } = useContext(BandTone);
  return (
    <svg
      className={`absolute inset-x-0 top-0 block w-full h-4 -translate-y-[15px] ${flip ? "-scale-x-100" : ""}`}
      viewBox="0 0 1200 16"
      preserveAspectRatio="none"
      aria-hidden="true"
    >
      <path d={`${CREST}V16H0Z`} className="fill-(--band)" />
      <path d={CREST} fill="none" className="stroke-sand/40" strokeWidth={1.5} vectorEffect="non-scaling-stroke" />
    </svg>
  );
}

function Band({ id, labelledby, children }: { id?: string; labelledby: string; children: ReactNode }) {
  const { tone } = useContext(BandTone);
  // --band is this band's ground; the edge above it is drawn in the same colour.
  const ground = tone === "raised" ? "[--band:var(--deep)]" : "[--band:var(--ink)]";
  return (
    <section
      id={id}
      aria-labelledby={labelledby}
      className={`relative px-gutter py-section scroll-mt-16 bg-(--band) [--claw-gap:var(--band)] ${ground}`}
    >
      <Edge />
      {children}
    </section>
  );
}

/** The claw and its eyes carry the mark's weight high; settle it onto the row's centre line. */
const SETTLE = "translate-y-0.5";
/** Landing-only nudges of the wordmark's claw: it sits a little low and may overflow its box. */
const CLAW = "-mb-[0.1em] overflow-visible";

/** A claw-marked list of short notes, on ink or on seafoam. */
function Notes({ notes, ground = "ink" }: { notes: Array<{ title: string; body: ReactNode }>; ground?: "ink" | "seafoam" }) {
  const body = ground === "ink" ? `text-chalk-dim ${CODE_ON_INK}` : `text-chalk-dim ${CODE_ON_INK}`;
  return (
    <ul className="mt-9 grid gap-6.5">
      {notes.map((n) => (
        <li key={n.title} className="grid grid-cols-[38px_minmax(0,1fr)] gap-x-3.5 gap-y-1">
          <ClawMark size={30} className="row-span-2 mt-0.5" />
          <h3 className="font-serif font-semibold text-[1.3rem] leading-[1.3]">{n.title}</h3>
          <p className={`max-w-[52ch] ${body}`}>{n.body}</p>
        </li>
      ))}
    </ul>
  );
}

/** A code inset: a label row and a block of highlighted lines. */
function CodeInset({ label, note, children, className = "" }: { label: ReactNode; note: ReactNode; children: ReactNode; className?: string }) {
  return (
    <figure className={`${INSET} ${className}`} aria-label={typeof label === "string" ? label : undefined}>
      <figcaption className={`${INSET_LABEL} ${CODE_ON_INK}`}>
        <span>{label}</span>
        <span>{note}</span>
      </figcaption>
      <pre className={INSET_PRE}>
        <code>{children}</code>
      </pre>
    </figure>
  );
}

const K = ({ children }: { children: ReactNode }) => <span className="text-coral">{children}</span>;
const F = ({ children }: { children: ReactNode }) => <span className="text-seafoam">{children}</span>;
const S = ({ children }: { children: ReactNode }) => <span className="text-sand">{children}</span>;
const T = ({ children }: { children: ReactNode }) => <span className="text-palm">{children}</span>;
const C = ({ children }: { children: ReactNode }) => <span className="text-chalk-dim italic">{children}</span>;

export function LandingPage({ docsUrl }: LandingPageProps) {
  return (
    <div className="landing min-h-screen bg-ink text-chalk font-sans leading-[1.6] antialiased overflow-x-clip">
      <nav className="sticky top-0 z-20 h-16 bg-ink border-b border-line" aria-label="Chelabase">
        <div className={`${COLUMN} flex items-center justify-between gap-4 h-full`}>
          <a href="/" className={`flex items-center text-[1.25rem] ${FOCUS}`}>
            <Wordmark visualClassName={SETTLE} clawClassName={`h-[1.32em] ${CLAW}`} />
          </a>
          <div className="flex gap-2.5">
            <a className={plate({ primary: true, small: true })} href={docsHref("using/quickstart", docsUrl)}>
              Get started
            </a>
            <a className={`${plate({ small: true })} max-[560px]:hidden`} href={GITHUB_URL}>
              Visit GitHub
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
            Manage, collaborate, iterate. On Postgres.
          </p>
          <p className="mt-2.5 max-w-160 text-chalk-dim text-[1.125rem] text-pretty">
            An open-source Postgres platform you run yourself. Branch, merge and restore databases, give your team access
            Postgres itself enforces, and query from your app with a typed client. Chelabase's own services are written in
            Rust.
          </p>
          <Actions docsUrl={docsUrl} className="mt-6 short:mt-4.5" />
        </div>
        <ShoreScene />
      </header>

      <main>
        {SECTIONS.map((Section, i) => (
          <BandTone.Provider key={i} value={{ tone: i % 2 ? "ink" : "raised", flip: i % 2 === 1 }}>
            <Section docsUrl={docsUrl} />
          </BandTone.Provider>
        ))}
      </main>

      <footer className={`${COLUMN} flex flex-wrap items-center justify-between gap-x-8 gap-y-4 pt-5.5 pb-7 text-chalk-dim text-[0.92rem]`}>
        <div className="flex items-center gap-2">
          <Wordmark className="text-[1.15rem]" visualClassName={SETTLE} clawClassName={`h-[1.32em] ${CLAW}`} />
          <span>· self-hosted preview</span>
        </div>
        <span>A hosted version is planned. Drawings on this page are illustrations.</span>
      </footer>
    </div>
  );
}

type SectionProps = { docsUrl?: string };

const SECTIONS: Array<(p: SectionProps) => ReactNode> = [Install, Pieces, Compare, Manage, Collaborate, YourApp, Iterate, Landfall];

/* ---------- Everything in one place ---------- */

interface Piece {
  id: string;
  icon: LucideIcon;
  title: string;
  body: ReactNode;
  /** The docs page it links to, e.g. `architecture/gateway`; none while it's coming soon. */
  to?: string;
  soon?: boolean;
}

// The platform's parts, most important first; what's still coming goes last. Each card opens its docs page.
const PIECES: Piece[] = [
  {
    id: "databases",
    icon: Database,
    title: "Databases",
    body: "Postgres 17, each database with its own tenant and timeline and each branch its own compute, on Neon's storage engine. Postgres clients connect through its proxy with login roles.",
    to: "using/projects-and-databases",
  },
  {
    id: "studio",
    icon: LayoutDashboard,
    title: "Studio",
    body: "The web console for all of it: projects, databases, the branch tree, the table and SQL editors, members, keys and your app's users.",
    to: "using/studio",
  },
  {
    id: "auth",
    icon: KeyRound,
    title: "Auth",
    body: "Signs in Studio members and your app's own users, with helpers for row-level security policies.",
    to: "architecture/app-users",
  },
  {
    id: "realtime",
    icon: Radio,
    title: "Realtime",
    body: "Pushes changed rows and presence over a WebSocket as keys only, so each row is read again as the viewer's role.",
    to: "architecture/realtime",
  },
  {
    id: "gateway",
    icon: DoorOpen,
    title: "Gateway",
    body: "The single front door. It checks every request's credential, a member's token or a data API key, and forwards it to the right service.",
    to: "architecture/gateway",
  },
  {
    id: "orchestrator",
    icon: Waypoints,
    title: "Orchestrator",
    body: "Serves the Management API, decides who may do what, and keeps the platform's own state.",
    to: "architecture/orchestrator",
  },
  {
    id: "worker",
    icon: Cog,
    title: "Worker",
    body: "Runs the orchestrator's jobs: creating, branching, resetting, merging and deleting databases.",
    to: "architecture/orchestrator",
  },
  {
    id: "rest",
    icon: Braces,
    title: "Rest",
    body: "The data API. It turns a JSON query into SQL and runs it as the key's role, so row-level security decides what each caller sees.",
    to: "architecture/data-api",
  },
  {
    id: "cli",
    icon: SquareTerminal,
    title: "CLI",
    body: "A script you install for quick setup and management from the terminal: generate types, run migrations and more. Not available yet.",
    soon: true,
  },
  {
    id: "storage",
    icon: Archive,
    title: "Object storage",
    body: "S3-compatible storage for your app's files, planned on SeaweedFS, which already holds Neon's data in the stack. Not available yet.",
    soon: true,
  },
];

/** Below 560px a card becomes a tile: icon over name, no description. */
const CARD =
  "relative grid h-full grid-cols-[44px_minmax(0,1fr)] content-start gap-x-4 gap-y-1 p-5 border rounded-xl " +
  "max-[560px]:grid-cols-1 max-[560px]:gap-y-3 max-[560px]:p-4";
const CARD_LINK =
  "group border-line bg-deep transition-[background-color,border-color,translate] duration-200 ease-design " +
  "hover:-translate-y-px hover:border-sand/45 hover:bg-ink-lit focus-visible:outline-2 focus-visible:outline-offset-3 focus-visible:outline-sand";

function PieceBody({ piece: p }: { piece: Piece }) {
  return (
    <>
      <span
        className={`row-span-2 size-11 grid place-items-center rounded-xl max-[560px]:row-span-1 max-[560px]:size-10 ${
          p.soon ? "border-[1.5px] border-dashed border-coral text-coral" : "bg-seafoam/12 text-seafoam"
        }`}
        aria-hidden="true"
      >
        <p.icon size={22} strokeWidth={1.6} />
      </span>
      <h3 className="flex flex-wrap items-baseline gap-x-3 gap-y-1.5 mt-2 font-serif font-semibold leading-[1.3] text-[1.25rem] max-[560px]:mt-0 max-[560px]:text-[1.1rem]">
        {p.title}
        {p.soon && (
          <span className="inline-block px-2.25 py-px border-[1.5px] border-dashed border-coral rounded-full font-sans text-[0.8rem] font-semibold text-coral whitespace-nowrap">
            Coming soon
          </span>
        )}
        {p.to && (
          <ArrowUpRight
            size={18}
            strokeWidth={1.8}
            className="ml-auto self-center text-chalk-dim max-[560px]:absolute max-[560px]:top-4 max-[560px]:right-4 transition-[color,translate] duration-200 group-hover:text-sand group-hover:translate-x-0.5 group-hover:-translate-y-0.5"
            aria-hidden="true"
          />
        )}
      </h3>
      <p className={`max-w-[54ch] text-chalk-dim ${CODE_ON_INK} max-[560px]:hidden`}>{p.body}</p>
    </>
  );
}

function Pieces({ docsUrl }: SectionProps) {
  return (
        <Band labelledby="pieces-title">
      <div className={SECTION_INNER}>
        <h2 id="pieces-title" className={SECTION_TITLE}>
          Everything in one place.
        </h2>
        <p className={`${SECTION_LEDE} text-chalk-dim`}>
          Every Chelabase backend service is built in Rust, and so is Neon's storage engine under Postgres: small
          native services that run together on one machine.
        </p>
        <ul className="mt-11 grid grid-cols-2 gap-4 max-[900px]:grid-cols-1 max-[560px]:grid-cols-2 max-[560px]:gap-3 max-[560px]:mt-8">
          {PIECES.map((p) => (
            <li key={p.id}>
              {p.to ? (
                <a href={docsHref(p.to, docsUrl)} className={`${CARD} ${CARD_LINK}`}>
                  <PieceBody piece={p} />
                </a>
              ) : (
                <div className={`${CARD} border-dashed border-coral/45`}>
                  <PieceBody piece={p} />
                </div>
              )}
            </li>
          ))}
        </ul>
        <p className="mt-7 text-chalk-dim italic font-serif">The crab is a nod to Ferris, Rust's mascot. Chela is Greek for claw.</p>
      </div>
    </Band>
  );
}

/* ---------- Manage ---------- */

const MANAGE_NOTES = [
  {
    title: "Structure changes you can read first",
    body: "The table editor stages column, index and constraint changes, shows them as the exact SQL with a dry run, and applies them in one transaction.",
  },
  {
    title: "A SQL editor with a guard",
    body: "It asks before risky statements, runs as the owner or as a role you pick, and keeps saved queries and history for the project.",
  },
  {
    title: "Credentials you can see again",
    body: "Login role passwords are encrypted at rest and can be revealed later, so a lost connection string doesn't mean a reset.",
  },
  {
    title: "Security in plain view",
    body: "Studio flags tables the data API can reach without row-level security, and says when you're running as the owner.",
  },
];

function Manage() {
  return (
    <Band id="manage" labelledby="manage-title">
      <div className={`${SECTION_INNER} ${SPLIT}`}>
        <div>
          <h2 id="manage-title" className={SECTION_TITLE}>
            Manage it from one console.
          </h2>
          <p className={`${SECTION_LEDE} text-chalk-dim`}>
            Studio holds the whole database: tables, SQL, roles, keys and your app's users. A change shows as the SQL it
            will run, before it runs.
          </p>
          <Notes notes={MANAGE_NOTES} />
        </div>
        <CodeInset label="Table editor · review changes" note="illustration" className="sticky top-22 max-[900px]:static">
          <C>-- 2 changes to public.orders, applied in one transaction</C>
          {"\n"}
          <K>alter table</K> public.orders{"\n"}
          {"  "}
          <K>add column</K> shipped_at <T>timestamptz</T>;{"\n"}
          <K>create index</K> orders_status_idx{"\n"}
          {"  "}
          <K>on</K> public.orders (status);{"\n\n"}
          <C>-- dry run: ok</C>
        </CodeInset>
      </div>
    </Band>
  );
}

/* ---------- Collaborate ---------- */

const COLLABORATE_NOTES = [
  {
    title: "Teams with live grants",
    body: "Change a team's grant and every member changes with it. Per-database overrides narrow it for one person.",
  },
  {
    title: "Protected branches",
    body: "Protect a branch from delete and reset. Protecting first reviews any code an untrusted role could run in an Admin's session.",
  },
  {
    title: "Work side by side",
    body: "The table editor shows live rows and who else is looking, and a save fails rather than overwrite someone's change.",
  },
  {
    title: "An access log",
    body: "Every invite, grant, approval and removal is recorded with who did it and what changed, never secrets.",
  },
];

function Collaborate() {
  return (
    <Band id="collaborate" labelledby="collaborate-title">
      <div className={`${SECTION_INNER} ${SPLIT} grid-cols-[minmax(0,1.05fr)_minmax(0,1fr)]`}>
        <div>
          <h2 id="collaborate-title" className={SECTION_TITLE}>
            Bring your team in.
          </h2>
          <p className={`${SECTION_LEDE} text-chalk-dim`}>
            Invite people, group them into teams, and grant each team the Postgres roles it may act as. Postgres' own
            privileges decide the rest, so a schema can belong to one team.
          </p>
          <Notes notes={COLLABORATE_NOTES} />
        </div>
        <CodeInset
          label="SQL editor · a schema per team"
          note="from the docs"
          className="sticky top-22 order-first max-[900px]:static max-[900px]:order-none"
        >
          <K>create role</K> billing <K>nologin</K>;{"\n"}
          <K>create schema</K> billing <K>authorization</K> billing;{"\n\n"}
          <C>-- Members › Teams › billing:</C>
          {"\n"}
          <C>-- Developer on the project, acting as billing.</C>
          {"\n"}
          <C>-- Its members change this schema; nobody else can.</C>
        </CodeInset>
      </div>
    </Band>
  );
}

/* ---------- Iterate ---------- */

const TERM = "font-serif font-semibold text-[1.25rem]";
const DEFINITION = "mt-1 max-w-[52ch] text-chalk-dim";

function Iterate() {
  return (
    <Band id="iterate" labelledby="branch-title">
      <div
        className={`${SECTION_INNER} grid grid-cols-[minmax(0,0.9fr)_minmax(0,1.4fr)] gap-16 items-start max-[900px]:grid-cols-1`}
      >
        <div>
          <h2 id="branch-title" className={SECTION_TITLE}>
            Branch it. Reset it. Restore it. Merge it.
          </h2>
          <p className={`${SECTION_LEDE} text-chalk-dim`}>
            Every database starts with one branch, main. Try a change on a copy of real data, then bring it home.
          </p>
          <dl className="mt-8 grid gap-5">
            <div>
              <dt className={TERM}>Branch</dt>
              <dd className={DEFINITION}>
                Copy any branch as it is now, or as it was at any point in its history window (seven days by default).
                The copy is copy-on-write, so it's ready at once, with its own endpoint and password.
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
            <div>
              <dt className={TERM}>Merge</dt>
              <dd className={DEFINITION}>
                Move schema changes from a branch into its parent with a three-way merge. Resolve conflicts, rehearse
                on a copy of the target, then apply. Rows never move.
              </dd>
            </div>
          </dl>
        </div>
        <TideGauge />
      </div>
    </Band>
  );
}

/* ---------- Your app ---------- */

const APP_NOTES = [
  {
    title: "Auth for your app's users",
    body: "Sign-up, sign-in and sessions, with helpers for row-level security policies.",
  },
  {
    title: "Server rendering",
    body: "Browser and server clients see the same signed-in user through cookies, and the refresh token stays HttpOnly.",
  },
  {
    title: "Plain Postgres too",
    body: (
      <>
        Migrations, ORMs and <code>psql</code> connect over the Postgres protocol with a login role.
      </>
    ),
  },
];

function YourApp() {
  return (
    <Band id="your-app" labelledby="app-title">
      <div className={`${SECTION_INNER} ${SPLIT}`}>
        <div>
          <h2 id="app-title" className={SECTION_TITLE}>
            Build your app on a typed API.
          </h2>
          <p className={`${SECTION_LEDE} text-chalk-dim ${CODE_ON_INK}`}>
            One endpoint, <code>POST /data/v1/query</code>, and a client with types fetched from your schema. Queries run
            as <code>anon</code> or <code>authenticated</code>, so row-level security decides what each user sees.
          </p>
          <Notes notes={APP_NOTES} />
        </div>
        <CodeInset label="Your server, with @chelabase/client" note={<code>orders.ts</code>}>
          <K>import</K> {"{ "}
          <F>createServerClient</F>
          {" }"} <K>from</K> <S>"@chelabase/client/server"</S>;{"\n"}
          <K>import</K> {"{ "}
          <K>type</K> <T>Database</T>, types {"}"} <K>from</K> <S>"./db"</S>;{"\n"}
          <C>// ./db comes from GET /data/v1/types</C>
          {"\n\n"}
          <K>const</K> db = <F>createServerClient</F>&lt;<T>Database</T>&gt;({"{ "}url, key, types, cookies{" }"});{"\n"}
          <K>const</K> user = <K>await</K> db.auth.<F>getUser</F>();{"\n\n"}
          <K>const</K> paid = <K>await</K> db{"\n"}
          {"  "}.<F>from</F>(<S>"orders"</S>){"\n"}
          {"  "}.<F>select</F>(<S>"id"</S>, <S>"total"</S>){"\n"}
          {"  "}.<F>where</F>({"{ "}status: <S>"paid"</S>
          {" }"});{"\n"}
          <C>// paid: {"{ id: bigint; total: string }"}[]</C>
        </CodeInset>
      </div>
    </Band>
  );
}

/* ---------- Run it yourself ---------- */

const INSTALL_STEPS: Array<{ title: string; body: ReactNode }> = [
  {
    title: "Clone and configure",
    body: (
      <>
        Copy <code>.env.example</code> to <code>.env</code>. The defaults run on one machine.
      </>
    ),
  },
  {
    title: "Start it",
    body: (
      <>
        <code>docker compose up -d --wait</code>. You need Docker Engine 26 and Compose 2.30 or newer, nothing else.
      </>
    ),
  },
  {
    title: "Claim it",
    body: "Read the setup code from the logs and create the first account with it: that account is the instance's admin.",
  },
  { title: "Create a database", body: "Open Studio, add a database, branch it and query it." },
];

function InstallTerminal({ className = "" }: { className?: string }) {
  return (
    <CodeInset label="Your terminal" note="from the README" className={className}>
      <F>git</F> clone https://github.com/chelabase/chelabase.git{"\n"}
      <F>cd</F> chelabase/infra/compose{"\n"}
      <F>cp</F> .env.example .env{"\n"}
      <F>docker</F> compose up -d --wait{"\n"}
      <F>docker</F> compose logs init-secrets <C># your setup code</C>
      {"\n\n"}
      <C># then open http://studio.chelabase.localhost:8000</C>
    </CodeInset>
  );
}

function Install() {
  return (
    <Band id="install" labelledby="install-title">
      <div className={`${SECTION_INNER} ${SPLIT}`}>
        <div>
          <h2 id="install-title" className={SECTION_TITLE}>
            Run it on your machine.
          </h2>
          {/* a dotted line threads the step numbers */}
          <ol className="relative mt-8 grid gap-6.5 before:absolute before:left-4.5 before:top-5 before:bottom-5 before:border-l-2 before:border-dotted before:border-shell before:opacity-70">
            {INSTALL_STEPS.map((s, i) => (
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
        <InstallTerminal className="sticky top-22 max-[900px]:static" />
      </div>
    </Band>
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

const COLUMNS = ["Chelabase", "Supabase", "Firebase", "Neon", "PlanetScale Postgres", "PlanetScale MySQL"] as const;

type Marks = [Mark, Mark, Mark, Mark, Mark, Mark];

const ROWS: Array<{ label: string; marks: Marks }> = [
  { label: "Postgres", marks: ["yes", "yes", "other", "yes", "yes", "other"] },
  { label: "Copy-on-write branches", marks: ["yes", "other", "no", "yes", "no", "no"] },
  { label: "Schema merge between branches", marks: ["yes", "partial", "no", "no", "no", "yes"] },
  { label: "Point-in-time restore", marks: ["yes", "yes", "yes", "yes", "yes", "no"] },
  { label: "Protected branches", marks: ["yes", "no", "no", "yes", "partial", "yes"] },
  { label: "Self-hostable", marks: ["yes", "partial", "no", "no", "no", "no"] },
  { label: "Typed query API", marks: ["yes", "yes", "other", "yes", "no", "no"] },
  { label: "App-user auth, server rendering included", marks: ["yes", "yes", "yes", "yes", "no", "no"] },
  { label: "Team access through Postgres roles", marks: ["yes", "other", "other", "other", "other", "other"] },
  { label: "Table and SQL editors", marks: ["yes", "yes", "partial", "yes", "partial", "partial"] },
  { label: "Live rows", marks: ["yes", "yes", "yes", "partial", "no", "no"] },
  { label: "Object storage", marks: ["soon", "yes", "yes", "yes", "no", "no"] },
];

function Compare() {
  return (
    <Band labelledby="compare-title">
      <div className={SECTION_INNER}>
        <h2 id="compare-title" className={SECTION_TITLE}>
          How it compares.
        </h2>
        <p className={`${SECTION_LEDE} text-chalk-dim`}>
          What each platform offers today, from its own docs. Benchmarks come later.
        </p>
        {/* a wireframe, not a result */}
        <div className="mt-10 p-7 border-2 border-dashed border-line-strong rounded-2xl bg-ink">
          <p className="inline-block mb-5.5 px-3.5 py-1.5 border-2 border-coral rounded-md text-coral font-serif font-bold text-[0.95rem] -rotate-[1.5deg]">
            Comparison preview — benchmarks coming later
          </p>
          {/* focusable so a keyboard can scroll the columns a phone cuts off */}
          <div className={`overflow-x-auto ${FOCUS}`} tabIndex={0} role="region" aria-label="Comparison table">
            <table className="w-full min-w-200 border-collapse text-[0.97rem]" aria-label="Comparison preview">
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
                      <th scope="row" className={`${CELL} ${rule} w-[20%] font-semibold text-chalk`}>
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
            From each product's docs, October 2026. Supabase's merge requests and Neon's realtime are in beta; Neon's
            schema diff compares branches but doesn't merge them. PlanetScale merges schema through deploy requests on
            MySQL (Vitess) only.
          </p>
        </div>
      </div>
    </Band>
  );
}

/* ---------- Final call to action ---------- */

function Landfall({ docsUrl }: SectionProps) {
  return (
    <Band labelledby="landfall-title">
      <div className="max-w-250 mx-auto grid grid-cols-[minmax(160px,0.7fr)_minmax(0,1.3fr)] gap-14 items-center max-[900px]:grid-cols-1 max-[900px]:justify-items-start">
        <svg className="w-full h-auto max-w-80 max-[900px]:max-w-50" viewBox="0 0 240 190" aria-hidden="true">
          <CrabFigure />
        </svg>
        <div className="min-w-0 w-full">
          <h2 id="landfall-title" className="font-serif font-semibold text-[2.75rem] leading-[1.05] tracking-tight">
            Start with the Quickstart.
          </h2>
          <p className="mt-3.5 max-w-[52ch] text-chalk-dim text-[1.125rem] text-pretty">
            Create a database, add a table, and query it from your app with the typed client.
          </p>
          <Actions docsUrl={docsUrl} />
        </div>
      </div>
    </Band>
  );
}
