# Studio

Web console (`apps/studio`). Stack: Vite, React, TanStack Router, Base UI, Vitest.

## Tests

- Logic (hooks, reducers, validation, access checks) gets unit tests next to the code.
- Layout and styling get no unit tests. A browser check or Impeccable's finish review
  covers them when they matter.
- While a Base UI modal (dialog, menu) is open, everything outside it is `aria-hidden`, so
  a role query for the page behind it finds nothing. Grab those elements before the modal
  opens, or assert after it closes.

## Browser

Workbench Studio is at `http://studio.<slug>.chelabase.localhost:<port>`;
`just workbench status` prints the URL. If input dies after a form sign-in, it's Chrome's
hidden "Save password?" prompt: navigate to `about:blank` and back.
