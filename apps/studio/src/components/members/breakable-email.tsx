import { Fragment } from "react";

/** An email with break opportunities after `@` and after each dot, so a narrow column wraps at sensible points. */
export function breakableEmail(email: string) {
  const parts = email.split(/(?<=[@.])/);
  return parts.map((part, i) => (
    <Fragment key={i}>
      {part}
      {i < parts.length - 1 && <wbr />}
    </Fragment>
  ));
}
