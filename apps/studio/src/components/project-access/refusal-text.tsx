import type { ReactNode } from "react";

/** A sentence with `backticked` names drawn as mono spans, never as literal backticks. */
export function RefusalText({ text }: { text: string }) {
  const parts = text.split("`");
  // An unpaired backtick is plain text.
  if (parts.length % 2 === 0) return <>{text}</>;
  const out: ReactNode[] = parts.map((p, i) =>
    i % 2 === 1 ? (
      <span key={i} className="font-mono text-[13px]">
        {p}
      </span>
    ) : (
      p
    ),
  );
  return <>{out}</>;
}
