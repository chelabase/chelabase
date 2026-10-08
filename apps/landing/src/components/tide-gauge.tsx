import { type CSSProperties, useId, useState } from "react";

/** The Branching section's instrument: pick branch, reset or restore, and a point in main's history. */

type Mode = "branch" | "reset" | "restore";

const MODES: Array<{ value: Mode; label: string }> = [
  { value: "branch", label: "Branch" },
  { value: "reset", label: "Reset" },
  { value: "restore", label: "Restore" },
];

const WINDOW_HOURS = 168; // the default 7-day history window
const LEFT = 40;
const NOW = 520;
const RIGHT = 624;
const MAIN_Y = 190;

const xAt = (hoursAgo: number) => NOW - (hoursAgo / WINDOW_HOURS) * (NOW - LEFT);

function describeHoursAgo(h: number): string {
  if (h === 0) return "now";
  if (h < 24) return h === 1 ? "1 hour ago" : `${h} hours ago`;
  const days = Math.floor(h / 24);
  const hours = h % 24;
  const d = days === 1 ? "1 day" : `${days} days`;
  return hours === 0 ? `${d} ago` : `${d} ${hours} hours ago`;
}

const style = (vars: Record<string, string | number>) => vars as CSSProperties;

/** The caption row of an inset (code panels and this instrument). */
export const INSET_LABEL =
  "flex justify-between gap-3 px-4 py-2.5 border-b border-line font-serif italic text-[0.92rem] text-chalk-dim";

/** The keyboard ring, as on every focusable thing on the landing. */
export const FOCUS = "focus-visible:outline-2 focus-visible:outline-offset-3 focus-visible:outline-sand";

const MODE_TAB = [
  "block -mb-px px-2 pt-2.5 pb-2.75 border border-transparent border-b-line-strong rounded-t-lg",
  "text-center font-serif font-semibold text-[1.05rem] text-chalk-dim",
  "transition-[color,border-color] duration-160 ease-lagoon",
  "peer-hover:text-chalk",
  "peer-checked:text-chalk peer-checked:border-line-strong peer-checked:border-b-ink",
  "peer-checked:bg-ink peer-checked:bg-[linear-gradient(var(--sand),var(--sand))] peer-checked:bg-top peer-checked:bg-[length:100%_3px] peer-checked:bg-no-repeat",
  "peer-focus-visible:outline-2 peer-focus-visible:-outline-offset-4 peer-focus-visible:outline-sand",
].join(" ");

function channel(x: number, y: number) {
  const bend = Math.min(60, RIGHT - x - 10);
  return `M${x} ${MAIN_Y} C ${x + bend * 0.6} ${MAIN_Y} ${x + bend * 0.5} ${y} ${x + bend} ${y} L ${RIGHT} ${y}`;
}

export function TideGauge() {
  const id = useId();
  const [mode, setMode] = useState<Mode>("branch");
  const [hoursAgo, setHoursAgo] = useState(72);
  const when = describeHoursAgo(hoursAgo);
  const forkX = mode === "reset" ? NOW : xAt(hoursAgo);
  const oldForkX = xAt(hoursAgo);

  const caption =
    mode === "branch"
      ? `preview is a copy-on-write copy of main as it was ${when}. It shares main's pages until either side writes.`
      : mode === "reset"
        ? "Reset moves preview to main's latest state. Its connection string and password stay the same."
        : `Restore takes main back to ${when} on a new timeline. The old one stays as a backup branch; the connection string and password don't change.`;

  return (
    <figure className="m-0 border border-line-strong rounded-xl overflow-hidden bg-ink">
      <div className={INSET_LABEL}>
        <span>Tide gauge: main's last 7 days</span>
        <span>Illustration, not live data</span>
      </div>

      <fieldset className="flex gap-1.5 m-0 px-4 pt-3.5 pb-0 border-0 border-b border-line-strong min-w-0">
        <legend className="sr-only">What happens</legend>
        {MODES.map((m) => (
          <label key={m.value} className="relative flex-1">
            <input
              className="peer absolute inset-0 m-0 opacity-0 cursor-pointer"
              type="radio"
              name={`${id}-mode`}
              value={m.value}
              checked={mode === m.value}
              onChange={() => setMode(m.value)}
            />
            <span className={MODE_TAB}>{m.label}</span>
          </label>
        ))}
      </fieldset>

      <svg className="block w-full h-auto" viewBox="0 0 640 300" aria-hidden="true">
        <defs>
          <pattern id={`${id.replace(/:/g, "")}-hazard`} width="8" height="8" patternUnits="userSpaceOnUse" patternTransform="rotate(45)">
            <line x1="0" y1="0" x2="0" y2="8" stroke="var(--coral)" strokeWidth="1.2" opacity="0.5" />
          </pattern>
        </defs>
        {/* main's dredged water */}
        <path d={`M${LEFT} ${MAIN_Y} L ${RIGHT} ${MAIN_Y}`} stroke="var(--seafoam)" strokeOpacity={0.12} strokeWidth={30} fill="none" />
        {/* Day ticks */}
        {Array.from({ length: 8 }, (_, d) => {
          const x = xAt(d * 24);
          return (
            <g key={d}>
              <line x1={x} y1={MAIN_Y + 10} x2={x} y2={MAIN_Y + 18} stroke="var(--line-strong)" />
              <text x={x} y={MAIN_Y + 40} textAnchor="middle" className="font-serif italic" fontSize="13" fill="var(--chalk-dim)">
                {d === 0 ? "now" : `${d}d`}
              </text>
            </g>
          );
        })}

        {/* main */}
        {mode === "restore" ? (
          <>
            <path d={`M${LEFT} ${MAIN_Y} L ${oldForkX} ${MAIN_Y}`} stroke="var(--seafoam)" strokeWidth={3} fill="none" />
            <rect x={oldForkX} y={MAIN_Y - 15} width={Math.max(0, NOW - oldForkX)} height={30} fill={`url(#${id.replace(/:/g, "")}-hazard)`} />
            <path d={`M${oldForkX} ${MAIN_Y} L ${NOW} ${MAIN_Y}`} stroke="var(--coral)" strokeWidth={1.5} strokeDasharray="3 5" fill="none" />
            <text x={NOW} y={MAIN_Y - 22} textAnchor="end" className="font-serif italic" fontSize="14" fill="var(--coral)">
              main-before-restore-… (backup)
            </text>
            <path
              d={`M${oldForkX} ${MAIN_Y} C ${oldForkX + 30} ${MAIN_Y} ${oldForkX + 30} 250 ${oldForkX + 60} 250 L ${RIGHT} 250`}
              stroke="var(--seafoam)"
              strokeOpacity={0.14}
              strokeWidth={24}
              fill="none"
            />
            <path
              key="restore"
              d={`M${oldForkX} ${MAIN_Y} C ${oldForkX + 30} ${MAIN_Y} ${oldForkX + 30} 250 ${oldForkX + 60} 250 L ${RIGHT} 250`}
              stroke="var(--seafoam)"
              strokeWidth={3}
              fill="none"
              pathLength={1}
              className="survey"
              style={style({ "--len": 1 })}
            />
            <text x={RIGHT - 4} y={240} textAnchor="end" className="font-serif italic" fontSize="17" fill="var(--seafoam)">
              main
            </text>
          </>
        ) : (
          <>
            <path d={`M${LEFT} ${MAIN_Y} L ${NOW} ${MAIN_Y}`} stroke="var(--seafoam)" strokeWidth={3} fill="none" />
            <path d={`M${NOW} ${MAIN_Y} L ${RIGHT} ${MAIN_Y}`} stroke="var(--seafoam)" strokeWidth={1.5} strokeDasharray="2 6" fill="none" />
            <text x={LEFT} y={MAIN_Y - 12} className="font-serif italic" fontSize="17" fill="var(--seafoam)">
              main
            </text>
          </>
        )}

        {/* preview */}
        {mode === "reset" && (
          <path d={channel(oldForkX, 100)} stroke="var(--coral)" strokeWidth={1.5} strokeDasharray="3 6" fill="none" opacity={0.45} />
        )}
        {mode !== "restore" && (
          <>
            <path d={channel(forkX, 100)} stroke="var(--seafoam)" strokeOpacity={0.14} strokeWidth={24} fill="none" />
            <path
              key={mode}
              d={channel(forkX, 100)}
              stroke="var(--coral)"
              strokeWidth={2.5}
              strokeLinecap="round"
              fill="none"
              pathLength={1}
              className="survey"
              style={style({ "--len": 1 })}
            />
            <text x={RIGHT - 4} y={88} textAnchor="end" className="font-serif italic" fontSize="17" fill="var(--coral)">
              preview
            </text>
          </>
        )}

        {/* the fork point */}
        <circle
          key={`${mode}-${hoursAgo}`}
          cx={forkX}
          cy={MAIN_Y}
          r={18}
          fill="none"
          stroke="var(--sand)"
          strokeWidth={1.5}
          className="ripple"
        />
        <circle cx={forkX} cy={MAIN_Y} r={5} fill="var(--ink)" stroke="var(--sand)" strokeWidth={2} />
      </svg>

      <div className="grid gap-2 px-5 pt-4 pb-5 border-t border-line">
        <label htmlFor={`${id}-when`} className="flex justify-between gap-3 text-[0.95rem] text-chalk-dim">
          <span>Point in time</span>
          <output htmlFor={`${id}-when`} className="font-serif italic text-sand">
            {mode === "reset" ? "main's latest" : when}
          </output>
        </label>
        <input
          id={`${id}-when`}
          className={`tide-staff appearance-none w-full h-7 m-0 cursor-pointer disabled:cursor-not-allowed disabled:opacity-40 ${FOCUS}`}
          type="range"
          min={0}
          max={WINDOW_HOURS}
          step={6}
          value={hoursAgo}
          disabled={mode === "reset"}
          aria-valuetext={when}
          style={{ direction: "rtl" }}
          onChange={(e) => setHoursAgo(Number(e.target.value))}
        />
      </div>
      <p className="m-0 px-5 pb-4.5 min-h-[3.2em] text-chalk text-[0.98rem]" aria-live="polite">
        {caption}
      </p>
    </figure>
  );
}
