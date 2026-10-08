# @chelabase/lagoon

The single source of the lagoon design: tokens, fonts, the Tailwind preset, the Scalar skin and the Fumadocs theme. Dark only. Source-only package (no build): apps import the CSS files.

## Files

- `tokens.css`: the font imports and one `:root` block: the palette (`--ink`, `--deep`, `--channel`, `--shallows`, `--sand`, `--sand-lit`, `--seafoam`, `--coral`, `--palm`, `--chalk`, `--chalk-dim`, `--line`, `--line-strong`, `--input`), the claw colours, the HTTP method tones (`--method-get/put/patch`), `--radius`, `color-scheme: dark`, and the font stacks `--lagoon-font-sans/serif/mono`.
- `base.css`: box-sizing, scrollbars, `body` colours and font, `::selection`.
- `tailwind.css`: Tailwind v4 mapping: `--color-*` for the palette, `--ease-lagoon`, radii, and Tailwind's `--font-sans/serif/mono` onto the `--lagoon-font-*` stacks.
- `scalar.css`: the Scalar API reference skin, every rule under `.lagoon-scalar` (the wrapper) or `body > .scalar-app` (Scalar's teleported popovers).
- `fumadocs.css`: the Fumadocs theme: every `--color-fd-*` colour on the tokens (`@theme` and `.dark`; callouts and diffs by the one-meaning rule), hairlines instead of shadows on popovers and the search dialog, radii from `--radius`, code blocks on `--deep`, serif headings and mono code, and the OpenAPI method badges in Studio's tones (scoped to fumadocs-openapi's badge classes).

## One meaning per colour

Coral is hazard, seafoam is yes, sand is action and focus, palm is reserved. The method tones (`--method-get/put/patch`) are only for HTTP methods. Hairline, not shadow.

## Fonts (D8)

Only this package imports fonts or defines font stacks. Apps use `var(--font-sans|serif|mono)` or Tailwind's `font-sans|serif|mono`. Never define `--font-*` in `tokens.css`: Tailwind's `--font-sans: var(--lagoon-font-sans)` would otherwise cycle.

## Consuming

```css
@import "tailwindcss";
@import "@chelabase/lagoon/tokens.css";
@import "@chelabase/lagoon/base.css";
@import "@chelabase/lagoon/tailwind.css";
```

Studio then imports `@chelabase/lagoon/scalar.css?inline` for the API page. The docs site adds `fumadocs-ui/css/preset.css` and the other Fumadocs presets, but no Fumadocs colour theme such as `neutral.css` (`fumadocs.css` declares every Fumadocs colour), after `tailwind.css`, then `@chelabase/lagoon/fumadocs.css` last. Add `"@chelabase/lagoon": "workspace:*"` to the app's dependencies.

## Tests

`pnpm --filter @chelabase/lagoon test` runs seven files:

- `tokens.test.ts`: token values against Studio's at 34ec766, dark `color-scheme`, no font properties in `tokens.css`, Tailwind's font mapping.
- `contrast.test.ts`: WCAG contrast of text and method tones (4.5:1), method tones on their 10% badge tint (5.4:1), Fumadocs callout and diff colours (4.5:1) and control borders (3:1).
- `scalar-scope.test.ts`: the Scalar skin's scoping, theming of popovers and the token mapping.
- `fumadocs.test.ts`: the Fumadocs skin maps every colour onto tokens, with the badge remaps (including the sand Deprecated override), hairline popovers and serif/mono fonts.
- `fonts-single-source.test.ts`: only the package imports fonts or defines font stacks, across every app (D8).
- `fonts-matcher.test.ts`: the matcher behind that guard.
- `duplicates.test.ts`: no custom property is declared twice in one block.
