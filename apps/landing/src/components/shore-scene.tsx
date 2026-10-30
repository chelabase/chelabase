import { RotateCcw } from "lucide-react";
import { useId, useLayoutEffect, useMemo, useRef, useState } from "react";
import { CrabFigure, PalmTree } from "@chelabase/design/crab-marks";

/** The hero's dusk shore, drawn to the width it is given: crabs walk main and fork off down branches. Synthetic. */

type Pt = [number, number];

interface Trail {
  name: string;
  pts: Pt[];
  cum: number[];
  total: number;
  /** distance along main where it forks off */
  forkAt: number;
}

/** Branch names in order; the last branch is always `staging`, the one that merges. */
const BRANCH_NAMES = ["preview", "search"];
const branchName = (i: number, n: number) => (i === n - 1 ? "staging" : BRANCH_NAMES[i]);

/** The scene's crabs, branches, sun and palms live in the page's centred column; sand and water run full width. */
const SCENE_MAX = 1240;

/** How many branches fit the column: two on phones, three on desktops (the column caps the width). */
export function forkCount(contentWidth: number): number {
  return contentWidth < 700 ? 2 : 3;
}

function measure(pts: Pt[]) {
  const cum = [0];
  for (let i = 1; i < pts.length; i++) cum.push(cum[i - 1] + Math.hypot(pts[i][0] - pts[i - 1][0], pts[i][1] - pts[i - 1][1]));
  return { cum, total: cum[cum.length - 1] };
}

function pointAt(pts: Pt[], cum: number[], d: number): Pt {
  if (d <= 0) return pts[0];
  if (d >= cum[cum.length - 1]) return pts[pts.length - 1];
  let i = 1;
  while (cum[i] < d) i++;
  const t = (d - cum[i - 1]) / (cum[i] - cum[i - 1] || 1);
  return [pts[i - 1][0] + (pts[i][0] - pts[i - 1][0]) * t, pts[i - 1][1] + (pts[i][1] - pts[i - 1][1]) * t];
}

function cubic(a: Pt, b: Pt, c: Pt, d: Pt, steps: number): Pt[] {
  const out: Pt[] = [];
  for (let i = 0; i <= steps; i++) {
    const t = i / steps;
    const u = 1 - t;
    out.push([
      u * u * u * a[0] + 3 * u * u * t * b[0] + 3 * u * t * t * c[0] + t * t * t * d[0],
      u * u * u * a[1] + 3 * u * u * t * b[1] + 3 * u * t * t * c[1] + t * t * t * d[1],
    ]);
  }
  return out;
}

const toD = (pts: Pt[]) => pts.map((p, i) => `${i ? "L" : "M"}${p[0].toFixed(1)} ${p[1].toFixed(1)}`).join(" ");

/** The next section's wave edge reaches this far up into the bottom of the scene; nothing is drawn there. */
const TIDE_PX = 44;

function buildScene(W: number, H: number) {
  const horizon = H * 0.3;
  const beachTop = H * 0.47;
  // the part of the beach crabs and names may use: above the incoming tide
  const floor = H - TIDE_PX;
  const mainY = beachTop + (floor - beachTop) * 0.46;
  const yAt = (x: number) => mainY + 5 * Math.sin(x / 150);

  const mainPts: Pt[] = [];
  for (let x = -40; x <= W + 40; x += 16) mainPts.push([x, yAt(x)]);
  const main = measure(mainPts);

  // the centred column everything that moves is laid out in
  const L = Math.max(24, (W - SCENE_MAX) / 2);
  const CW = W - 2 * L;
  const n = forkCount(CW);
  const headX = L + CW * 0.94;
  const trails: Trail[] = [];
  for (let i = 0; i < n; i++) {
    const fx = L + CW * (0.12 + (i * 0.56) / Math.max(1, n - 1));
    const up = i % 2 === 0;
    const by = up ? beachTop + (floor - beachTop) * 0.16 : beachTop + (floor - beachTop) * 0.86;
    const run = Math.max(90, CW * 0.09);
    const bx = fx + run;
    // branches end well short of where main's crab stops, so the two never crowd
    const ex = Math.min(L + CW * 0.86, bx + Math.max(80, CW * (n > 2 ? 0.1 : 0.16)));
    const a: Pt = [fx, yAt(fx)];
    const pts = [...cubic(a, [fx + run * 0.5, a[1]], [bx - run * 0.5, by], [bx, by], 28), [ex, by] as Pt];
    const m = measure(pts);
    // distance along main to the fork point
    let forkAt = 0;
    while (forkAt < main.cum.length && mainPts[forkAt][0] < fx) forkAt++;
    trails.push({ name: branchName(i, n), pts, ...m, forkAt: main.cum[Math.min(forkAt, main.cum.length - 1)] });
  }

  const atX = (x: number) => {
    let i = 0;
    while (i < mainPts.length && mainPts[i][0] < x) i++;
    return main.cum[Math.min(i, main.cum.length - 1)];
  };
  const headAt = atX(headX);
  // after staging is merged, main's crab walks on a little further
  const head2At = atX(L + CW * 0.985);
  // merge: staging's crab hops from the end of its branch down onto main, just behind main's crab
  const staging = trails[n - 1];
  const e = staging.pts[staging.pts.length - 1];
  const px = Math.max(e[0] + 30, headX - 46);
  const p: Pt = [px, yAt(px)];
  const hopPts = cubic(e, [e[0] + 24, e[1]], [p[0] - 18, p[1] - 24], p, 18);
  const hop = { pts: hopPts, ...measure(hopPts) };
  // main's crab enters just left of the column; the trail before that is main's past, already there
  const startAt = atX(L - 110);

  const sunR = Math.min(190, Math.max(70, H * 0.28));
  return {
    W,
    H,
    floor,
    horizon,
    beachTop,
    L,
    CW,
    main: { pts: mainPts, ...main },
    headAt,
    head2At,
    startAt,
    hop,
    trails,
    sun: { cx: L + CW - Math.max(130, CW * 0.15), cy: horizon + sunR * 0.18, r: sunR },
    crabScale: Math.min(0.5, Math.max(0.3, H / 900)),
  };
}

const NAME = "font-serif italic text-[17px] transition-opacity ease-design";
/** A branch's name, in ink on the sand. */
const BRANCH_NAME = `${NAME} fill-ink duration-400`;
/** What just happened to a branch (reset, merged), in the claw's dark shell. */
const EVENT_NAME = `${NAME} fill-shell-dark font-semibold duration-240`;

function wave(y: number, W: number, amp: number, len: number) {
  const pts: Pt[] = [];
  for (let x = -20; x <= W + 20; x += 20) pts.push([x, y + amp * Math.sin(x / len)]);
  return pts;
}

export function ShoreScene() {
  const id = useId().replace(/:/g, "");
  const box = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ w: 1440, h: 460 });
  const [run, setRun] = useState(0);

  useLayoutEffect(() => {
    const el = box.current;
    if (!el) return;
    const read = () => {
      const w = Math.round(el.clientWidth) || 1440;
      const h = Math.round(el.clientHeight) || 460;
      setSize((s) => (s.w === w && s.h === h ? s : { w, h }));
    };
    read();
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(read);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const scene = useMemo(() => buildScene(size.w, size.h), [size.w, size.h]);
  const crabRefs = useRef<(SVGGElement | null)[]>([]);
  const maskRefs = useRef<(SVGPathElement | null)[]>([]);
  const resetLabel = useRef<SVGTextElement | null>(null);
  const nameRefs = useRef<(SVGTextElement | null)[]>([]);
  const mergeLabel = useRef<SVGTextElement | null>(null);
  const burrow = useRef<SVGEllipseElement | null>(null);

  // Crab and trail state is written straight to the DOM each frame; React only lays the scene out.
  // A trail is revealed exactly up to its crab's centre (butt-capped mask), so footprints only ever
  // appear behind a crab, never ahead of it.
  useLayoutEffect(() => {
    const { main, trails, headAt, head2At, startAt, hop, crabScale } = scene;
    const s0 = crabScale;
    const s1 = crabScale * 0.66;
    const place = (i: number, p: Pt, scale: number, bob = 0) => {
      crabRefs.current[i]?.setAttribute("transform", `translate(${p[0].toFixed(1)} ${(p[1] + bob).toFixed(1)}) scale(${scale})`);
    };
    const reveal = (i: number, f: number) => {
      const el = maskRefs.current[i];
      if (el) el.style.strokeDashoffset = String(1 - Math.max(0, Math.min(1, f)));
      // a branch's name appears once its crab has set off
      if (i > 0) nameRefs.current[i - 1]?.setAttribute("opacity", f > 0 ? "1" : "0");
    };
    // staging (the last branch) is merged at the end: its crab hops onto main and burrows in,
    // and main's crab, now carrying staging's state, walks on. Staging's trail stays: the branch is kept.
    const del = hop ? trails.length - 1 : -1;
    // the hop's footprint trail is drawn after main's and the branches'
    const hopIdx = trails.length + 1;

    /** The end state. `merged` tucks staging's crab into main; reduced motion shows every crab instead. */
    const finish = (merged: boolean) => {
      const endAt = merged && del >= 0 ? head2At : headAt;
      place(0, pointAt(main.pts, main.cum, endAt), s0);
      reveal(0, endAt / main.total);
      trails.forEach((t, i) => {
        const gone = merged && i === del;
        place(i + 1, t.pts[t.pts.length - 1], gone ? 0.001 : s1);
        reveal(i + 1, 1);
      });
      resetLabel.current?.setAttribute("opacity", "0");
      // once merged, the hop's footprints, the burrow hole and the note stay as the record of it
      const shown = merged && del >= 0 ? "1" : "0";
      if (hop) reveal(hopIdx, shown === "1" ? 1 : 0);
      mergeLabel.current?.setAttribute("opacity", shown);
      burrow.current?.setAttribute("opacity", shown);
    };

    const reduce = typeof window.matchMedia === "function" && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    if (reduce || typeof requestAnimationFrame === "undefined") {
      finish(false);
      return;
    }

    const BIG = 0.34; // px per ms
    const SMALL = 0.24;
    const BURROW = 900; // ms for the staging crab to sink into the sand
    const start = performance.now();
    const branchStart = trails.map((t) => (t.forkAt - startAt) / BIG);
    const branchEnd = trails.map((t, i) => branchStart[i] + t.total / SMALL);
    const lastEnd = Math.max((headAt - startAt) / BIG, ...branchEnd);
    const resetBack = lastEnd + 700;
    const resetOut = resetBack + trails[0].total / SMALL + 450;
    const resetDone = resetOut + trails[0].total / SMALL;
    const delBack = resetDone + 600;
    const delBurrow = del >= 0 && hop ? delBack + hop.total / SMALL : resetDone;
    const delEnd = del >= 0 ? delBurrow + BURROW : resetDone;
    const done = delEnd + 600;
    const easeOut = (x: number) => 1 - (1 - x) * (1 - x);
    let frame = 0;

    const tick = (now: number) => {
      const t = now - start;
      // main's crab: walk to its stop; once staging burrows, walk on a little further
      let bigD = Math.min(headAt, startAt + t * BIG);
      if (del >= 0 && t >= delBurrow) bigD = headAt + (head2At - headAt) * easeOut(Math.min(1, (t - delBurrow) / BURROW));
      const walking = bigD < headAt || (t >= delBurrow && t < delEnd);
      place(0, pointAt(main.pts, main.cum, bigD), s0, walking ? Math.sin(t / 55) * 1.6 : 0);
      reveal(0, bigD / main.total);

      trails.forEach((tr, i) => {
        let d: number;
        let scale = s1;
        let sink = 0;
        if (i === 0 && t >= resetBack && t < resetDone) {
          if (t < resetBack + tr.total / SMALL) d = tr.total - (t - resetBack) * SMALL;
          else if (t < resetOut) d = 0;
          else d = Math.min(tr.total, (t - resetOut) * SMALL);
        } else if (i === del && hop && t >= delBack) {
          // hop from the branch's end onto main, then burrow in; staging's own trail stays
          const k = Math.min(1, Math.max(0, (t - delBurrow) / BURROW));
          scale = Math.max(0.001, s1 * (1 - k));
          sink = 10 * k;
          const hd = Math.min(hop.total, (t - delBack) * SMALL);
          reveal(hopIdx, hd / hop.total);
          const moving = hd < hop.total;
          place(i + 1, pointAt(hop.pts, hop.cum, hd), scale, (moving ? Math.sin(t / 45 + i) * 1.2 : t < delEnd ? Math.sin(t / 25) * 1.5 : 0) + sink);
          reveal(i + 1, 1);
          return;
        } else {
          d = Math.min(tr.total, Math.max(0, (t - branchStart[i]) * SMALL));
        }
        const active = t >= branchStart[i];
        const moving = active && d > 0 && d < tr.total;
        const wiggle = moving ? Math.sin(t / 45 + i) * 1.2 : i === del && t >= delBurrow && t < delEnd ? Math.sin(t / 25) * 1.5 : 0;
        place(i + 1, active ? pointAt(tr.pts, tr.cum, d) : tr.pts[0], active ? scale : 0.001, wiggle + sink);
        reveal(i + 1, d / tr.total);
      });

      resetLabel.current?.setAttribute("opacity", t >= resetBack && t < resetDone ? "1" : "0");
      mergeLabel.current?.setAttribute("opacity", del >= 0 && t >= delBurrow ? "1" : "0");
      burrow.current?.setAttribute("opacity", del >= 0 && t >= delBurrow ? String(Math.min(1, (t - delBurrow) / 300)) : "0");
      if (t < done) frame = requestAnimationFrame(tick);
      else finish(true);
    };

    // start state before the first paint
    place(0, pointAt(main.pts, main.cum, startAt), s0);
    reveal(0, startAt / main.total);
    trails.forEach((tr, i) => {
      place(i + 1, tr.pts[0], 0.001);
      reveal(i + 1, 0);
    });
    if (hop) reveal(hopIdx, 0);
    mergeLabel.current?.setAttribute("opacity", "0");
    burrow.current?.setAttribute("opacity", "0");
    frame = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(frame);
  }, [scene, run]);

  const { W, H, L, CW, floor, horizon, beachTop, main, trails, sun, hop } = scene;
  const lagoonTop = wave(horizon, W, 4, 90);
  const beachEdge = wave(beachTop, W, 7, 120);
  const fill = (pts: Pt[]) => `${toD(pts)} L ${W + 20} ${H + 20} L -20 ${H + 20} Z`;
  const palmScale = Math.min(1.35, Math.max(0.55, H / 520));
  const first = trails[0];
  const firstFork = first.pts[0];

  return (
    <div
      className="relative flex-auto min-h-80 -mt-14 [--claw-gap:var(--sand)] max-[560px]:flex-none max-[560px]:h-90 max-[560px]:mt-3"
      ref={box}
    >
      <svg
        className="absolute inset-0 block size-full"
        width={W}
        height={H}
        viewBox={`0 0 ${W} ${H}`}
        role="img"
        aria-label={`Illustration: a crab walks main along the beach while smaller crabs fork off down ${trails.length} branches; one branch then resets, and staging's schema is merged into main, its crab burrowing into the sand.`}
      >
        <defs>
          {[main, ...trails, ...(hop ? [hop] : [])].map((t, i) => (
            <mask key={i} id={`${id}-m${i}`} maskUnits="userSpaceOnUse" x={-40} y={0} width={W + 80} height={H}>
              <path
                ref={(el) => {
                  maskRefs.current[i] = el;
                }}
                d={toD(t.pts)}
                pathLength={1}
                fill="none"
                stroke="white"
                strokeWidth={46}
                strokeLinecap="butt"
                strokeDasharray="1 1"
                strokeDashoffset={0}
              />
            </mask>
          ))}
        </defs>

        {/* sun, lagoon, beach */}
        <circle cx={sun.cx} cy={sun.cy} r={sun.r} fill="var(--sand)" />
        <path d={fill(lagoonTop)} fill="var(--seafoam)" />
        <g fill="none" stroke="var(--ink)" strokeOpacity={0.18} strokeWidth={2.5} strokeLinecap="round">
          {Array.from({ length: Math.ceil(W / 260) }, (_, i) => {
            const x = 60 + i * 260 + (i % 2) * 90;
            const y = horizon + (beachTop - horizon) * (0.35 + (i % 3) * 0.18);
            return <path key={i} d={`M${x} ${y} q 22 -6 44 0 t 44 0`} />;
          })}
        </g>
        <path d={fill(beachEdge)} fill="var(--sand)" />
        <path d={toD(beachEdge)} fill="none" stroke="var(--tip)" strokeWidth={3} opacity={0.8} />

        {/* palms */}
        <g color="var(--palm)">
          <g transform={`translate(${L + CW - 40} ${beachTop + 26}) scale(${palmScale})`}>
            <PalmTree lean={-1} />
          </g>
          <g transform={`translate(${L + CW - 140} ${beachTop + 18}) scale(${palmScale * 0.78})`}>
            <PalmTree lean={1} />
          </g>
        </g>

        {/* trails: wet sand, then footprints */}
        {[main, ...trails, ...(hop ? [hop] : [])].map((t, i) => (
          <g key={i} mask={`url(#${id}-m${i})`}>
            <path d={toD(t.pts)} fill="none" stroke="var(--wet-sand)" strokeWidth={i === 0 ? 22 : 15} strokeLinecap="round" strokeLinejoin="round" />
            <path d={toD(t.pts)} fill="none" stroke="var(--ink)" strokeOpacity={0.55} strokeWidth={3.5} strokeLinecap="round" strokeDasharray="0.1 10" transform="translate(0 -3.5)" />
            <path d={toD(t.pts)} fill="none" stroke="var(--ink)" strokeOpacity={0.55} strokeWidth={3.5} strokeLinecap="round" strokeDasharray="0.1 10" strokeDashoffset={5} transform="translate(0 3.5)" />
          </g>
        ))}

        {/* names */}
        <text x={L + 4} y={main.pts[0][1] - 18} className={BRANCH_NAME}>
          main
        </text>
        {trails.map((t, i) => {
          // name the branch just past its bend, under the trail, clear of where its crab stops
          const at = t.pts[28];
          return (
            <text
              key={t.name}
              ref={(el) => {
                nameRefs.current[i] = el;
              }}
              x={at[0] + 10}
              y={at[1] + 31 > floor ? at[1] - 14 : at[1] + 31}
              className={BRANCH_NAME}
            >
              {t.name}
            </text>
          );
        })}
        <text ref={resetLabel} x={firstFork[0]} y={firstFork[1] + 34} textAnchor="middle" className={EVENT_NAME} opacity={0}>
          reset
        </text>

        {hop && (
          <>
            <ellipse ref={burrow} cx={hop.pts[hop.pts.length - 1][0]} cy={hop.pts[hop.pts.length - 1][1] + 4} rx={14} ry={5} fill="var(--shell-dark)" opacity={0} />
            <text
              ref={mergeLabel}
              x={hop.pts[hop.pts.length - 1][0]}
              y={hop.pts[hop.pts.length - 1][1] + 34}
              textAnchor="middle"
              className={EVENT_NAME}
              opacity={0}
            >
              staging's schema merged into main
            </text>
          </>
        )}

        {/* crabs: main first, then one per branch */}
        {[0, ...trails.map((_, i) => i + 1)].map((i) => (
          <g
            key={i}
            ref={(el) => {
              crabRefs.current[i] = el;
            }}
          >
            <g transform="translate(-120 -172)">
              <CrabFigure />
            </g>
          </g>
        ))}
      </svg>
      {/* Above the next section's wave edge (TIDE_PX). */}
      <div className="absolute bottom-9 left-[max(var(--spacing-gutter),calc((100%-var(--container-landing))/2))] flex items-center gap-3.5 font-serif italic text-[0.92rem] text-ink">
        <button
          type="button"
          className="inline-flex items-center gap-1.5 min-h-8 px-3 border-[1.5px] border-ink rounded-full bg-transparent text-ink font-sans font-semibold not-italic text-[0.85rem] leading-none cursor-pointer hover:bg-ink/8 focus-visible:outline-2 focus-visible:outline-offset-3 focus-visible:outline-ink"
          onClick={() => setRun((r) => r + 1)}
        >
          <RotateCcw size={14} aria-hidden="true" />
          Replay
        </button>
        <span>Illustration, not your databases.</span>
      </div>
    </div>
  );
}
