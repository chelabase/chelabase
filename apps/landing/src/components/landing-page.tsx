import { createContext, useContext, useRef, type ReactNode } from "react";
import { useScrollHint } from "./scroll-hint";
import {
  Archive,
  ArrowUpRight,
  Braces,
  Cog,
  Database,
  DoorOpen,
  KeyRound,
  LayoutDashboard,
  Maximize2,
  Radio,
  SquareCode,
  Waypoints,
  X,
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

/** GitHub's mark (Octicons `mark-github`), drawn in the text colour. */
function GitHubMark() {
  return (
    <svg viewBox="0 0 16 16" className="size-[1.1em] shrink-0" fill="currentColor" aria-hidden="true">
      <path d="M8 0c4.42 0 8 3.58 8 8a8.013 8.013 0 0 1-5.45 7.59c-.4.08-.55-.17-.55-.38 0-.27.01-1.13.01-2.2 0-.75-.25-1.23-.54-1.48 1.78-.2 3.65-.88 3.65-3.95 0-.88-.31-1.59-.82-2.15.08-.2.36-1.02-.08-2.12 0 0-.67-.22-2.2.82-.64-.18-1.32-.27-2-.27-.68 0-1.36.09-2 .27-1.53-1.03-2.2-.82-2.2-.82-.44 1.1-.16 1.92-.08 2.12-.51.56-.82 1.28-.82 2.15 0 3.06 1.86 3.75 3.64 3.95-.23.2-.44.55-.51 1.07-.46.21-1.61.55-2.33-.66-.15-.24-.6-.83-1.23-.82-.67.01-.27.38.01.53.34.19.73.9.82 1.13.16.45.68 1.31 2.69.94 0 .67.01 1.3.01 1.49 0 .21-.15.45-.55.38A7.995 7.995 0 0 1 0 8c0-4.42 3.58-8 8-8Z" />
    </svg>
  );
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
        <GitHubMark />
        GitHub
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

function Band({ id, labelledby, className = "", children }: { id?: string; labelledby: string; className?: string; children: ReactNode }) {
  const { tone } = useContext(BandTone);
  // --band is this band's ground; the edge above it is drawn in the same colour.
  const ground = tone === "raised" ? "[--band:var(--deep)]" : "[--band:var(--ink)]";
  return (
    <section
      id={id}
      aria-labelledby={labelledby}
      className={`relative px-gutter py-section scroll-mt-16 bg-(--band) [--claw-gap:var(--band)] ${ground} ${className}`}
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

/** A claw-marked list of short notes, in one column or, above a screenshot, two (one below 900px). */
function Notes({ notes, columns = 1 }: { notes: Array<{ title: string; body: ReactNode }>; columns?: 1 | 2 }) {
  const body = `text-chalk-dim ${CODE_ON_INK}`;
  const grid = columns === 2 ? "mt-10 grid grid-cols-2 gap-x-14 gap-y-8 max-[900px]:grid-cols-1 max-[900px]:gap-y-6.5" : "mt-9 grid gap-6.5";
  return (
    <ul className={grid}>
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

/**
 * A Studio screenshot (made by `pnpm capture-studio`): rounded, captioned, loaded when it nears the viewport. Clicking
 * it opens it larger over the page; Esc, the close button or a click anywhere closes it again.
 */
function StudioShot({
  src,
  width,
  height,
  alt,
  caption,
  className = "",
}: {
  src: string;
  width: number;
  height: number;
  alt: string;
  caption: string;
  className?: string;
}) {
  const zoom = useRef<HTMLDialogElement>(null);
  const close = () => zoom.current?.close();
  return (
    <figure className={`m-0 min-w-0 ${className}`}>
      <button
        type="button"
        aria-label={`Enlarge: ${alt}`}
        onClick={() => zoom.current?.showModal()}
        className={`group relative block w-full p-0 border-0 rounded-xl bg-transparent cursor-zoom-in ${FOCUS}`}
      >
        <img
          src={src}
          width={width}
          height={height}
          alt={alt}
          loading="lazy"
          decoding="async"
          className="block w-full h-auto rounded-xl border border-line-strong bg-ink transition-[border-color] duration-200 ease-design group-hover:border-sand/45"
        />
        <span
          className="absolute top-3 right-3 grid place-items-center size-8 rounded-full bg-ink/85 text-chalk opacity-0 transition-opacity duration-200 ease-design group-hover:opacity-100 group-focus-visible:opacity-100"
          aria-hidden="true"
        >
          <Maximize2 size={15} strokeWidth={1.8} />
        </span>
      </button>
      <figcaption className="mt-2.5 text-[0.9rem] leading-snug text-chalk-dim">{caption}</figcaption>
      {/* Every click inside closes it: there is nothing to do in here but look. */}
      <dialog
        ref={zoom}
        aria-label={alt}
        onClick={close}
        className="m-auto p-0 border-0 bg-transparent max-w-[calc(100vw-2*var(--spacing-gutter))] max-h-none overflow-visible cursor-zoom-out backdrop:bg-ink/88"
      >
        <img
          src={src}
          width={width}
          height={height}
          alt={alt}
          loading="lazy"
          decoding="async"
          className="block w-auto h-auto max-w-full max-h-[calc(100svh-7rem)] rounded-xl border border-line-strong bg-ink shadow-[0_30px_80px_-30px] shadow-black/80"
        />
        <button
          type="button"
          aria-label="Close"
          onClick={close}
          className={`fixed top-4 right-4 grid place-items-center size-10 border-[1.5px] border-chalk rounded-full bg-ink text-chalk cursor-pointer hover:bg-ink-lit ${FOCUS}`}
        >
          <X size={18} strokeWidth={1.8} aria-hidden="true" />
        </button>
      </dialog>
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
              <GitHubMark />
              GitHub
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
            An open-source Postgres platform you can run yourself. Branch, merge and restore databases, give your team access
            Postgres itself enforces, and query from your app with a typed client. Its core services are written in Rust.
          </p>
          <Actions docsUrl={docsUrl} className="mt-6 short:mt-4.5" />
          {/* the same notice the README, docs and Studio carry */}
          <p className="mt-4 flex flex-wrap items-center gap-x-2.5 gap-y-1 text-[0.95rem] text-chalk-dim short:mt-3">
            <span className="px-1.5 py-px border border-sand/60 rounded-sm text-sand text-xs font-semibold uppercase tracking-wide">
              Alpha
            </span>
            <span>
              Not ready for production data yet: expect breaking changes, and{" "}
              <a href={`${GITHUB_URL}/issues`} className={`text-chalk underline underline-offset-3 decoration-sand/60 hover:decoration-sand ${FOCUS}`}>
                report what breaks
              </a>
              .
            </span>
          </p>
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
          <span>· self-hosted alpha</span>
        </div>
        <span>
          A hosted version is planned. Drawings on this page are illustrations.{" "}
          <a href="mailto:hello@chelabase.com" className={`text-chalk underline underline-offset-3 decoration-sand/60 hover:decoration-sand ${FOCUS}`}>
            hello@chelabase.com
          </a>
        </span>
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
    body: "Pushes changed rows and presence to Studio over a WebSocket as keys only, so each row is read again as the viewer's role.",
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
    id: "storage",
    icon: Archive,
    title: "Object storage",
    body: "S3-compatible buckets for your app's files, guarded by row-level security, with signed and public URLs, a Studio tab, the typed client and webhooks.",
    to: "using/storage",
  },
  {
    id: "functions",
    icon: SquareCode,
    title: "Edge functions",
    body: "Your own Workers-style TypeScript, with WebAssembly, run next to your database per branch and live within seconds of a save. Called over HTTP, on a schedule, or by table and storage events.",
    to: "using/functions",
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
          // On a phone tile the chip takes the corner a linked tile gives its arrow, so the name
          // keeps its line and every tile stays the same height. Under 400px it says only "Soon"
          // beside the icon; screen readers still hear "Coming soon".
          <span className="inline-block px-2.25 py-px border-[1.5px] border-dashed border-coral rounded-full font-sans text-[0.8rem] font-semibold text-coral whitespace-nowrap max-[560px]:absolute max-[560px]:top-4 max-[560px]:right-4 max-[560px]:px-1.75 max-[560px]:border max-[560px]:text-[0.7rem] max-[560px]:leading-[1.5]">
            <span className="max-[400px]:sr-only">Coming soon</span>
            <span className="hidden max-[400px]:inline" aria-hidden="true">
              Soon
            </span>
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
          Chelabase's core services are built in Rust, and so is Neon's storage engine under Postgres. Accounts live in a
          small internal Identity service on Better Auth. It all runs together on one machine.
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
    body: "It asks before risky statements, runs as the owner or as a role you pick, and keeps saved queries per database, shared with the project, with your run history in your browser.",
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
      <div className={SECTION_INNER}>
        <h2 id="manage-title" className={SECTION_TITLE}>
          Manage it from one console.
        </h2>
        <p className={`${SECTION_LEDE} text-chalk-dim`}>
          Studio holds the whole database: tables, SQL, roles, keys and your app's users. A change shows as the SQL it
          will run, before it runs.
        </p>
        <Notes notes={MANAGE_NOTES} columns={2} />
        <StudioShot
          className="mt-12"
          src="/screens/manage-table-editor.webp"
          width={1152}
          height={800}
          alt="Studio's table editor on sales.orders, Structure tab. A new column, shipped_at timestamptz, is staged in the Pending changes panel with its ALTER TABLE statement and a passed dry run."
          caption="A staged change shows its SQL and a dry run before anything runs."
        />
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
      <div className={SECTION_INNER}>
        <h2 id="collaborate-title" className={SECTION_TITLE}>
          Bring your team in.
        </h2>
        <p className={`${SECTION_LEDE} text-chalk-dim`}>
          Invite people, group them into teams, and grant each team the Postgres roles it may act as. Postgres' own
          privileges decide the rest, so a schema can belong to one team.
        </p>
        <Notes notes={COLLABORATE_NOTES} columns={2} />
        <StudioShot
          className="mt-12"
          src="/screens/collaborate-team-access.webp"
          width={1152}
          height={800}
          alt="Studio's Finance team page, Project access card for Acme: platform role Developer, acting as the Postgres role finance, with engineering, sales and support left unchecked."
          caption="The Finance team holds Developer on Acme, acting as finance: one grant for all its members."
        />
      </div>
    </Band>
  );
}

/* ---------- Iterate ---------- */

function Iterate() {
  return (
    <Band id="iterate" labelledby="branch-title">
      <div className={SECTION_INNER}>
        <h2 id="branch-title" className={SECTION_TITLE}>
          Branch it. Reset it. Restore it. Merge it.
        </h2>
        <p className={`${SECTION_LEDE} text-chalk-dim`}>
          Every database starts with one branch, main. Try a change on a copy of real data, then bring it home.
        </p>
        <div className="mt-10">
          <TideGauge />
        </div>
        <StudioShot
          className="mt-12"
          src="/screens/iterate-branches.webp"
          width={1152}
          height={790}
          alt="Studio's branches list for the Acme database: main, protected, and feature-discounts, branched from main, each with its endpoint and actions such as Reset to parent, Restore and Merge into main."
          caption="Protected main, and feature-discounts branched from it, ready to merge."
        />
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
            One endpoint, <code>POST /data/v1/query</code>, and a client with types fetched from your schema. Public-key
            queries run as <code>anon</code> or <code>authenticated</code>, so row-level security decides what each user sees.
            The client isn't on npm yet: use it from the repository.
          </p>
          <Notes notes={APP_NOTES} />
        </div>
        <CodeInset
          label="Your server, with @chelabase/client"
          note={<code>orders.ts</code>}
          className="sticky top-22 max-[900px]:static"
        >
          <K>import</K> {"{ "}
          <F>createServerClient</F>
          {" }"} <K>from</K> <S>"@chelabase/client/server"</S>;{"\n"}
          <K>import</K> {"{ "}
          <K>type</K> <T>Database</T>, types {"}"} <K>from</K> <S>"./db"</S>;{"\n"}
          <C>// ./db comes from GET /data/v1/types</C>
          {"\n\n"}
          <K>const</K> db = <F>createServerClient</F>&lt;<T>Database</T>&gt;({"{ "}url, key, types, cookies{" }"});{"\n"}
          <K>const</K> user = <K>await</K> db.auth.<F>getUser</F>();{"\n\n"}
          <K>const</K> mine = <K>await</K> db{"\n"}
          {"  "}.<F>from</F>(<S>"orders"</S>){"\n"}
          {"  "}.<F>select</F>(<S>"id"</S>, <S>"total"</S>){"\n"}
          {"  "}.<F>where</F>({"{ "}customer_id: user.id{" }"});{"\n"}
          <C>// sales.orders, only the signed-in customer's rows</C>
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
        <code>docker compose up -d --wait</code>. You need Docker Engine 26 and Compose 2.30 or newer, and room for about 26 containers.
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
    <CodeInset label="Your terminal" note="Install" className={className}>
      <F>git</F> clone https://github.com/chelabase/chelabase.git{"\n"}
      <F>cd</F> chelabase/infra/compose{"\n"}
      <F>cp</F> .env.example .env{"\n"}
      <F>docker</F> compose up -d --wait{"\n"}
      <F>docker</F> compose logs init-secrets <C># your setup code</C>
      {"\n\n"}
      <C># want a populated demo instead? on a fresh stack:</C>
      {"\n"}
      <C># docker compose -f docker-compose.yml -f docker-compose.example.yml up -d --wait</C>
      {"\n"}
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

type Mark = "yes" | "partial" | "soon" | "no";
/** A cell: a mark, or a product that solves it another way, named in a short note. */
type Cell = Mark | { other: string };

const MARK_TEXT: Record<Mark, string> = {
  yes: "Yes",
  partial: "Partly",
  soon: "Coming soon",
  no: "No",
};

/** The mark's colour; its glyph (`.mark--*::before`) is drawn in landing.css. A note is chalk, not
 *  chalk-dim, so another approach never reads as "No". */
const MARK_CLASS: Record<Mark | "other", string> = {
  yes: "text-seafoam",
  partial: "text-sand",
  soon: "text-coral",
  no: "text-chalk-dim",
  other: "text-chalk",
};

const CELL = "p-3.5 text-left";
/** The row names stay in view while a phone scrolls the products sideways: opaque, unlike the
 *  products' cells, with an inset edge since a collapsed border doesn't stick with the cell. */
const ROW_HEAD = "sticky left-0 z-1 min-w-36 min-[561px]:min-w-48 shadow-[inset_-1px_0_0_var(--line)] max-[560px]:pl-2.5";
/** Zebra rows; the sticky row name takes its row's ground. */
const STRIPE = ["bg-ink", "bg-channel"] as const;
/** Rules between columns only: the stripes separate the rows. */
const COLUMN_RULE = "border-l border-line";
/** Chelabase's column: a sand band edged in sand, from the header down. */
const BAND = "bg-sand/8 border-x border-sand/40";
const HEAD = "pb-3 border-b-2 border-b-line-strong font-serif font-semibold text-[1.1rem]";

const COLUMNS = ["Chelabase", "Supabase", "Firebase", "Neon", "PlanetScale Postgres", "PlanetScale MySQL"] as const;

type Cells = [Cell, Cell, Cell, Cell, Cell, Cell];

const ROWS: Array<{ label: string; cells: Cells }> = [
  { label: "Postgres", cells: ["yes", "yes", "yes", "yes", "yes", { other: "MySQL" }] },
  { label: "Copy-on-write branches", cells: ["yes", { other: "Schema-only branches" }, "no", "yes", "no", "no"] },
  { label: "Schema merge between branches", cells: ["yes", "partial", "no", "no", "no", "yes"] },
  { label: "Protected branches", cells: ["yes", "no", "no", "yes", "partial", "yes"] },
  { label: "Point-in-time restore", cells: ["yes", "yes", "yes", "yes", "yes", "no"] },
  { label: "Self-hostable", cells: ["yes", "partial", "no", "partial", "no", "no"] },
  {
    label: "Team access enforced by Postgres",
    cells: ["yes", { other: "Dashboard roles" }, { other: "Cloud IAM" }, { other: "Org roles" }, { other: "Org roles" }, { other: "Org roles" }],
  },
  { label: "Typed query API", cells: ["yes", "yes", "yes", "yes", "no", "no"] },
  { label: "App-user auth, with SSR", cells: ["yes", "yes", "yes", "yes", "no", "no"] },
  { label: "Table and SQL editors", cells: ["yes", "yes", "partial", "yes", "partial", "partial"] },
  { label: "Live rows in the editor", cells: ["yes", "no", "yes", "no", "no", "no"] },
  { label: "Realtime for your app", cells: ["soon", "yes", "yes", "yes", "no", "no"] },
  { label: "Object storage", cells: ["yes", "yes", "yes", "yes", "no", "no"] },
  { label: "Serverless functions", cells: ["yes", "yes", "yes", "yes", "no", "no"] },
];

function CompareCell({ cell }: { cell: Cell }) {
  const kind = typeof cell === "string" ? cell : "other";
  const text = typeof cell === "string" ? MARK_TEXT[cell] : cell.other;
  return <span className={`mark mark--${kind} flex items-center gap-2 whitespace-nowrap ${MARK_CLASS[kind]}`}>{text}</span>;
}

function Compare() {
  const scroller = useRef<HTMLDivElement>(null);
  const { scrollable, more } = useScrollHint(scroller);
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
        <div className="mt-10 p-7 border-2 border-dashed border-line-strong rounded-2xl bg-ink max-[560px]:p-4">
          <p className="inline-block mb-5.5 px-3.5 py-1.5 border-2 border-coral rounded-md text-coral font-serif font-bold text-[0.95rem] -rotate-[1.5deg]">
            Comparison preview — benchmarks coming later
          </p>
          <div className="relative">
            {/* focusable while it scrolls, so a keyboard can reach the columns a phone cuts off */}
            <div
              ref={scroller}
              className={`compare-scroll overflow-x-auto ${FOCUS}`}
              tabIndex={scrollable ? 0 : undefined}
              role="region"
              aria-label="Comparison table"
            >
              <table className="w-full min-w-200 border-collapse text-[0.97rem]">
                <caption className="sr-only">Comparison preview</caption>
                <thead>
                  <tr>
                    <td className={`${CELL} ${ROW_HEAD} bg-ink align-middle`} />
                    {COLUMNS.map((c, i) => (
                      <th key={c} scope="col" className={`${CELL} ${HEAD} ${i === 0 ? `${BAND} border-t` : COLUMN_RULE} align-bottom`}>
                        {/* Chelabase's own column is set in the wordmark, which is taller; the names align on the bottom. */}
                        {c === "Chelabase" ? <Wordmark visualClassName="translate-y-[0.24em]" clawClassName={`h-[1.32em] ${CLAW}`} /> : c}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {ROWS.map((r, row) => {
                    const stripe = STRIPE[row % 2];
                    return (
                      <tr key={r.label} className={stripe}>
                        <th scope="row" className={`${CELL} ${ROW_HEAD} ${stripe} align-middle w-[20%] font-semibold text-chalk`}>
                          {r.label}
                        </th>
                        {r.cells.map((cell, i) => (
                          <td key={COLUMNS[i]} className={`${CELL} align-middle ${i === 0 ? BAND : COLUMN_RULE}`}>
                            <CompareCell cell={cell} />
                          </td>
                        ))}
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
            {/* more columns to the right: a fade over the cut edge, gone once scrolled to the end */}
            <div
              aria-hidden="true"
              className={`pointer-events-none absolute inset-y-0 right-0 w-14 bg-linear-to-l from-ink to-transparent transition-opacity duration-200 ${more ? "opacity-100" : "opacity-0"}`}
            />
          </div>
          <p className="mt-4.5 max-w-[70ch] text-chalk-dim text-[0.95rem]">
            From each product's docs, November 2026. Neon's realtime and object storage are in beta; Neon's schema diff
            compares branches but doesn't merge them. Supabase merges a branch into production through a schema diff,
            and self-hosted Supabase runs one project without branching or point-in-time restore. PlanetScale merges
            schema through deploy requests on MySQL (Vitess) only. Firebase's Postgres is SQL Connect (Cloud SQL for
            PostgreSQL, with typed SDKs). Neon's storage engine is open source, but its control plane isn't. Supabase's
            functions are Deno Edge Functions, self-hosted Supabase included; Firebase's are Cloud Functions, which don't
            run at the edge; Neon's run Node.js on a branch. Chelabase runs functions in self-hosted mode only.
          </p>
        </div>
      </div>
    </Band>
  );
}

/* ---------- Final call to action ---------- */

function Landfall({ docsUrl }: SectionProps) {
  return (
    // Stacked on a phone, the crab, the call and the footer sit close: the last band needs less air
    // than a section between sections.
    <Band labelledby="landfall-title" className="max-[900px]:pt-10 max-[900px]:pb-10">
      <div className="max-w-250 mx-auto grid grid-cols-[minmax(160px,0.7fr)_minmax(0,1.3fr)] gap-14 items-center max-[900px]:grid-cols-1 max-[900px]:justify-items-start max-[900px]:gap-5">
        {/* cropped to the drawing (it fills x 23-223, y 29-172 of its 240 x 190 box), the box scaled to match */}
        <svg className="w-full h-auto max-w-72 max-[900px]:max-w-44" viewBox="16 22 214 157" aria-hidden="true">
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
