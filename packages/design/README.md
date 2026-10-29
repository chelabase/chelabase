# @chelabase/design

The single source of the Chelabase design system: tokens, both themes, fonts, the Tailwind preset, the Scalar skin and the Fumadocs theme. Dark only. Source-only package (no build): apps import the CSS files.

## Files

- `tokens.css`: the font imports and the two themes, which share the same unprefixed tokens. **Basalt** is the default, the bare `:root` block; **Lagoon** is the alternate, a `:root[data-theme="lagoon"]` block that redeclares only the neutrals, kept for a future theme picker (nothing sets it today). The tokens: the palette (`--ink`, `--deep`, `--channel`, `--shallows`, `--sand`, `--sand-lit`, `--wet-sand`, `--ink-lit`, `--seafoam`, `--coral`, `--palm`, `--chalk`, `--chalk-dim`, `--line`, `--line-strong`, `--input`), the claw colours (including `--claw-eye`), the HTTP method tones (`--method-get/put/patch`), `--radius` and `color-scheme: dark`. No font stacks: those live in `tailwind.css`.
- `src/crab-marks.tsx` (export `@chelabase/design/crab-marks`): the claw `ClawMark`, `Wordmark`, `CrabFigure` and `PalmTree` as React components, eye whites on `--claw-eye`. They use Tailwind utilities; the preset's `@source "./src"` makes any app that imports it generate them. `Wordmark` carries only the styling both apps share (serif, bold, tight, chalk; claw `h-[1.32em]`); apps add nudges through `className`, `visualClassName` and `clawClassName`. Needs `react` (peer).
- `base.css`: box-sizing, scrollbars, `body` colours and font, `::selection`.
- `tailwind.css`: Tailwind v4 mapping: `--color-*` for the palette, `--ease-lagoon`, radii, and the font stacks as Tailwind's `--font-sans/serif/mono` (literal values in a non-inline `@theme`, so Tailwind also emits them on `:root`).
- `scalar.css`: the Scalar API reference skin, every rule under `.lagoon-scalar` (the wrapper) or `body > .scalar-app` (Scalar's teleported popovers).
- `fumadocs.css`: the Fumadocs theme: every `--color-fd-*` colour on the tokens (`@theme` and `.dark`; callouts and diffs by the one-meaning rule), hairlines instead of shadows on popovers and the search dialog, radii from `--radius`, code blocks on `--deep`, serif headings and mono code, and the OpenAPI method badges in Studio's tones (scoped to fumadocs-openapi's badge classes).

## One meaning per colour

Coral is hazard, seafoam is yes, sand is action and focus, palm is reserved. The method tones (`--method-get/put/patch`) are only for HTTP methods. Hairline, not shadow.

## Fonts (D8)

Only this package imports fonts or defines font stacks. Apps use `var(--font-sans|serif|mono)` or Tailwind's `font-sans|serif|mono`. The stacks are defined once, in `tailwind.css`'s `@theme`; never define font properties in `tokens.css` or alias `--font-*` through another variable.

## Consuming

```css
@import "tailwindcss";
@import "@chelabase/design/tokens.css";
@import "@chelabase/design/base.css";
@import "@chelabase/design/tailwind.css";
```

Studio then imports `@chelabase/design/scalar.css?inline` for the API page. The docs site adds `fumadocs-ui/css/preset.css` and the other Fumadocs presets, but no Fumadocs colour theme such as `neutral.css` (`fumadocs.css` declares every Fumadocs colour), after `tailwind.css`, then `@chelabase/design/fumadocs.css` last. Add `"@chelabase/design": "workspace:*"` to the app's dependencies.

## Tests

`pnpm --filter @chelabase/design test` runs eight files:

- `tokens.test.ts`: token values (Basalt neutrals and the Lagoon block), dark `color-scheme`, no font properties in `tokens.css`, the font stacks in `tailwind.css`.
- `contrast.test.ts`: WCAG contrast of text and method tones (4.5:1), method tones on their 10% badge tint (5.4:1), Fumadocs callout and diff colours (4.5:1) and control borders (3:1).
- `scalar-scope.test.ts`: the Scalar skin's scoping, theming of popovers and the token mapping.
- `fumadocs.test.ts`: the Fumadocs skin maps every colour onto tokens, with the badge remaps (including the sand Deprecated override), hairline popovers and serif/mono fonts.
- `fonts-single-source.test.ts`: only the package imports fonts or defines font stacks, across every app (D8).
- `fonts-matcher.test.ts`: the matcher behind that guard.
- `crab-marks.test.tsx`: the wordmark's accessible name and shared classes, eye whites on `--claw-eye`, no hex, `rgb(` or `hsl(` literal in any `src` file.
- `duplicates.test.ts`: no custom property is declared twice in one block.
