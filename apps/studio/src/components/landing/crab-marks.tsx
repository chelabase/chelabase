import type { ElementType } from "react";

/** The Chelabase claw: an open coral chela drawn as the letter C (pincers opening right). 130-unit box.
 *  Its seams and rim are drawn in --claw-gap, the colour of the ground it sits on (ink by default),
 *  so they read as gaps and the claw looks the same on every background. */
export function ClawPaths() {
  return (
    <g stroke="var(--claw-gap, var(--ink))" strokeWidth={3.5} strokeLinejoin="round" strokeLinecap="round">
      <path d="M30 94 C 24 102 18 110 12 116 C 8 112 6 108 8 104 C 14 98 18 92 22 86 Z" fill="var(--shell-dark)" />
      <path
        d="M58 58 C 50 66 52 78 64 84 C 76 90 92 90 106 84 C 98 98 80 108 60 108 C 34 108 14 92 12 66 C 10 46 22 28 40 22 C 46 20 52 22 54 28 C 46 36 48 50 58 58 Z"
        fill="var(--shell)"
      />
      <path d="M44 24 C 58 10 82 8 104 22 C 108 26 106 30 100 30 C 86 26 70 28 62 40 C 58 46 56 52 58 58 C 48 54 40 42 44 24 Z" fill="var(--shell)" />
      <path d="M86 25 C 92 24 98 26 104 22 C 108 26 106 30 100 30 C 96 29 90 28 86 29 Z" fill="var(--tip)" />
      <path d="M86 88 C 94 88 100 87 106 84 C 102 90 96 95 88 98 Z" fill="var(--tip)" />
      <circle cx="50" cy="40" r="3.2" fill="var(--ink)" stroke="none" />
      <g fill="var(--tip)" stroke="none">
        <path d="M66 36 l3 6 l3 -7 Z" />
        <path d="M76 30 l3 6 l3 -7 Z" />
        <path d="M68 84 l3 -6 l3 6 Z" />
        <path d="M80 87 l3 -6 l3 6 Z" />
      </g>
      <path d="M24 56 C 26 44 32 36 40 32" fill="none" stroke="var(--shell-light)" strokeWidth={5} />
    </g>
  );
}

/** Two big floating eyes over the claw: the crab is just out of frame. */
function FloatingEyes() {
  return (
    <g stroke="var(--claw-gap, var(--ink))" strokeWidth={3}>
      <ellipse cx="18" cy="-2" rx="7" ry="8" fill="#fff6ee" />
      <ellipse cx="42" cy="-6" rx="7" ry="8" fill="#fff6ee" />
      <circle cx="19.8" cy="-0.4" r="3.4" fill="var(--ink)" stroke="none" />
      <circle cx="43.8" cy="-4.4" r="3.4" fill="var(--ink)" stroke="none" />
    </g>
  );
}

/** The mark: the claw C with floating eyes. Decorative; pair it with a text name. */
export function ClawMark({ size = 48, className }: { size?: number; className?: string }) {
  return (
    <svg className={className} width={size} height={(size * 142) / 130} viewBox="0 -18 130 142" aria-hidden="true" focusable="false">
      <ClawPaths />
      <FloatingEyes />
    </svg>
  );
}

/** "Chelabase", set as the claw C plus "helabase". Screen readers hear the whole name. Provisional mark. */
export function Wordmark({ as: Tag = "span", className }: { as?: ElementType; className?: string }) {
  return (
    <Tag className={`wordmark ${className ?? ""}`}>
      <span className="sr-only">Chelabase</span>
      <span className="wordmark__visual" aria-hidden="true">
        <ClawMark className="wordmark__claw" />
        <span>helabase</span>
      </span>
    </Tag>
  );
}

const LEGS = [
  "M86 128 L66 134 L56 150",
  "M90 136 L72 148 L66 164",
  "M98 141 L86 156 L84 172",
  "M154 128 L174 134 L184 150",
  "M150 136 L168 148 L174 164",
  "M142 141 L154 156 L156 172",
];

/** The walking crab, in a 240 x 190 box: the claw C is its right claw, a small claw on its left. */
export function CrabFigure() {
  return (
    <g>
      {/* small left claw */}
      <g transform="translate(220 0) scale(-1 1)" stroke="var(--ink)" strokeWidth={3.5} strokeLinejoin="round" strokeLinecap="round">
        <path d="M156 112 C 168 108 176 100 180 90" fill="none" strokeWidth={11} />
        <path d="M156 112 C 168 108 176 100 180 90" fill="none" stroke="var(--shell)" strokeWidth={6} />
        <path d="M182 92 C 194 90 200 78 196 68 C 194 62 188 60 184 63 C 187 69 186 75 181 78 C 177 80 175 86 182 92 Z" fill="var(--shell)" />
        <path d="M181 78 C 176 72 172 66 172 60 C 178 60 182 66 184 70 Z" fill="var(--tip)" />
      </g>
      <g fill="none" strokeLinecap="round" strokeLinejoin="round">
        <g stroke="var(--ink)" strokeWidth={10}>
          {LEGS.map((d) => (
            <path key={d} d={d} />
          ))}
        </g>
        <g stroke="var(--shell)" strokeWidth={5}>
          {LEGS.map((d) => (
            <path key={d} d={d} />
          ))}
        </g>
        <path d="M150 120 C 158 114 162 106 164 96" stroke="var(--ink)" strokeWidth={13} />
        <path d="M150 120 C 158 114 162 106 164 96" stroke="var(--shell)" strokeWidth={8} />
      </g>
      <g stroke="var(--ink)" strokeWidth={3.5} strokeLinejoin="round" strokeLinecap="round">
        <path d="M108 96 L102 70 M132 96 L138 70" fill="none" strokeWidth={9} />
        <path d="M108 96 L102 70 M132 96 L138 70" fill="none" stroke="var(--shell)" strokeWidth={4} />
        <ellipse cx="102" cy="64" rx="7.5" ry="9" fill="#fff6ee" />
        <ellipse cx="138" cy="64" rx="7.5" ry="9" fill="#fff6ee" />
        <circle cx="103.5" cy="65" r="3.6" fill="var(--ink)" stroke="none" />
        <circle cx="136.5" cy="65" r="3.6" fill="var(--ink)" stroke="none" />
        <path d="M78 122 C 78 100 96 90 120 90 C 144 90 162 100 162 122 C 162 136 146 146 120 146 C 94 146 78 136 78 122 Z" fill="var(--shell)" />
        <path d="M90 110 C 98 102 110 99 120 99 C 130 99 142 102 150 110" fill="none" stroke="var(--shell-light)" strokeWidth={5} />
        <path d="M112 126 C 116 131 124 131 128 126" fill="none" strokeWidth={3} />
      </g>
      <g transform="translate(144 20) scale(.74)">
        <ClawPaths />
      </g>
    </g>
  );
}

/** A palm silhouette, rooted at (0, 0), about 120 wide and 230 tall. Lean > 0 bends it right. */
export function PalmTree({ lean = 1 }: { lean?: number }) {
  const crown = { x: 26 * lean, y: -210 };
  const leaf = "M0 0 C 30 -22 74 -22 108 6 C 92 0 74 -2 58 4 C 40 -6 20 -4 0 8 Z";
  return (
    <g>
      <path
        d={`M0 0 C ${8 * lean} -70 ${20 * lean} -150 ${crown.x} ${crown.y}`}
        fill="none"
        stroke="currentColor"
        strokeWidth={11}
        strokeLinecap="round"
      />
      <g transform={`translate(${crown.x} ${crown.y})`} fill="currentColor">
        <path d={leaf} transform="rotate(-24)" />
        <path d={leaf} transform="rotate(14)" />
        <path d={leaf} transform="scale(-1 1) rotate(-20)" />
        <path d={leaf} transform="scale(-1 1) rotate(18)" />
        <path d={leaf} transform="rotate(-72) scale(.8)" />
        <circle r={7} />
      </g>
    </g>
  );
}
