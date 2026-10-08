import { explainMergeError } from "@/lib/merges";

/**
 * A merge's `error` for the reader: the worker's own sentence (after its code) as text, the lines
 * the planner refused under it, and anything else, Postgres's text included, as it was written.
 */
export function ErrorBody({ error, status }: { error: string; status?: string }) {
  const info = explainMergeError(error, status);
  if (info === null || info.kind === "other") {
    return <pre className="font-mono text-xs whitespace-pre-wrap">{error}</pre>;
  }
  return (
    <div className="space-y-1">
      {info.kind === "resolution_refused" ? (
        <ul className="list-disc pl-5">
          {info.refused.map((r) => (
            <li key={r}>{r}</li>
          ))}
        </ul>
      ) : (
        <p>{info.message}</p>
      )}
      {info.rest !== "" && <pre className="font-mono text-xs whitespace-pre-wrap">{info.rest}</pre>}
    </div>
  );
}
