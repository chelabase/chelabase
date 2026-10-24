import { useState, type ReactNode } from "react";
import { X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { changesWord, CollapseButton, CollapsedStrip, lockWords } from "@/components/table-editor/panel-bits";
import { cn } from "@/lib/utils";
import { escapeName } from "../hub/escape-name";
import { Square } from "./bits";
import { editText, listText, serverTargetText, type PrivilegeIndex } from "./model";
import type { Failure, Mismatch, PrivilegeEditorState } from "./use-privilege-editor";

/**
 * The role page's one right panel (the One Panel Rule): the pending changes, then their review (the
 * dry run's SQL and data API exposure), or what kept them from applying. Edits stay staged
 * through every refusal.
 */
export function PrivilegePanel({ editor, role }: { editor: PrivilegeEditorState; role: string }) {
  const [collapsed, setCollapsed] = useState(false);
  const { review, pending } = editor;
  if (!review && pending.length === 0) return null;
  const notApplied = review?.failure?.kind === "not_applied";
  const label = notApplied ? "Not applied" : review ? "Review changes" : "Pending changes";
  const heading = notApplied ? "Not applied" : review ? `Review · ${changesWord(review.edits.length)}` : `Pending changes · ${pending.length}`;
  if (collapsed) {
    return (
      <aside aria-label={label} className="flex lg:sticky lg:top-[52px] lg:h-[calc(100dvh-52px)]">
        <CollapsedStrip text={heading} onExpand={() => setCollapsed(false)} />
      </aside>
    );
  }
  return (
    <aside
      aria-label={label}
      className="flex w-full flex-col gap-3 border-t border-line-strong bg-deep p-4 lg:sticky lg:top-[52px] lg:h-[calc(100dvh-52px)] lg:w-[360px] lg:shrink-0 lg:overflow-y-auto lg:border-t-0 lg:border-l"
    >
      <div className="flex items-center gap-2.5">
        <h2 className="flex-1 text-base font-semibold">{heading}</h2>
        <CollapseButton onClick={() => setCollapsed(true)} />
      </div>
      {review ? <ReviewBody editor={editor} role={role} /> : <PendingBody editor={editor} />}
    </aside>
  );
}

function Footer({ children }: { children: ReactNode }) {
  return <div className="mt-auto flex justify-end gap-2 border-t border-line pt-3">{children}</div>;
}

function PendingBody({ editor }: { editor: PrivilegeEditorState }) {
  return (
    <>
      <p className="text-[13px] text-chalk-dim">Nothing is applied yet. Review runs a dry run and shows the SQL.</p>
      <ol className="border-t border-line">
        {editor.pending.map((e, i) => {
          const text = editText(e);
          return (
            <li key={text} className="grid grid-cols-[1.25rem_minmax(0,1fr)_1.5rem] items-start gap-2 border-b border-line py-2">
              <span className="pt-px text-xs text-chalk-dim">{i + 1}</span>
              <span className="font-mono text-[13px] break-words text-chalk">{text}</span>
              <button
                type="button"
                aria-label={`Remove ${text}`}
                onClick={() => editor.unstage(e)}
                className="flex size-6 items-center justify-center text-chalk-dim outline-none hover:text-chalk focus-visible:outline-2 focus-visible:outline-sand"
              >
                <X aria-hidden="true" size={14} strokeWidth={1.5} />
              </button>
            </li>
          );
        })}
      </ol>
      <Footer>
        <Button variant="ghost" onClick={editor.discard}>
          Discard
        </Button>
        <Button onClick={() => void editor.startReview()}>Review changes</Button>
      </Footer>
    </>
  );
}

function ReviewBody({ editor, role }: { editor: PrivilegeEditorState; role: string }) {
  const review = editor.review!;
  const { response, failure, running } = review;
  const n = review.edits.length;
  const statements = response?.statements ?? [];
  const exposure = response?.data_api_exposure ?? [];
  const passed = response !== null && response.would_succeed !== false;
  const canApply = passed && failure === null && !running && !review.applying;
  return (
    <>
      {running && (
        <p className="flex gap-2 text-[13px] text-chalk-dim">
          <span aria-hidden="true" className="mt-[8px] h-0.5 w-2 shrink-0 bg-chalk-dim" />
          Checking with a dry run…
        </p>
      )}
      {exposure.length > 0 && (
        <div className="flex gap-2 text-[13px] text-chalk">
          <Square tone="warn" />
          <p>
            <b className="font-semibold">Exposes data through the data API.</b>{" "}
            {exposure.map((x, i) => (
              <span key={`${x.op_index} ${i}`}>
                {i > 0 && "; "}
                change {x.op_index + 1} reaches <span className="font-mono">{x.chain.map(escapeName).join(" → ")}</span>
              </span>
            ))}
            . Every app user signed in as {listText([...new Set(exposure.map((x) => escapeName(x.chain[0] ?? "")))])} gets these grants.
          </p>
        </div>
      )}
      {failure?.kind === "exposes" && failure.chain.length > 0 && (
        <div className="flex gap-2 text-[13px] text-chalk">
          <Square tone="warn" />
          <p>
            <b className="font-semibold">Exposes data through the data API.</b>{" "}
            {failure.opIndex !== null ? `Change ${failure.opIndex + 1} reaches ` : "It reaches "}
            <span className="font-mono">{failure.chain.map(escapeName).join(" → ")}</span>.
          </p>
        </div>
      )}
      {failure && <FailureBox failure={failure} editor={editor} role={role} count={n} />}
      {review.partial && failure && (
        <>
          <div className="max-h-72 overflow-auto border border-line bg-ink px-3 py-2 font-mono text-xs leading-5 text-chalk">
            {(review.partial.response.statements ?? []).map((s, i) => (
              <code key={i} className={cn("block break-words whitespace-pre-wrap", s.skipped && "text-chalk-dim")}>
                {s.sql}
              </code>
            ))}
          </div>
          <p className="flex gap-2 text-[13px] text-seafoam">
            <Square tone="ok" />
            {`Dry run passed for ${review.partial.passed.length === 1 ? "change" : "changes"} ${listText(review.partial.passed.map((i) => String(i + 1)))}: without the refused one. Nothing is applied.`}
          </p>
        </>
      )}
      {statements.length > 0 && (
        <div className="max-h-72 overflow-auto border border-line bg-ink px-3 py-2 font-mono text-xs leading-5 text-chalk">
          {statements.map((s, i) => (
            <code key={i} className={cn("block break-words whitespace-pre-wrap", s.skipped && "text-chalk-dim")}>
              {s.sql}
            </code>
          ))}
        </div>
      )}
      {response && failure === null && (
        <p className={cn("flex gap-2 text-[13px]", passed ? "text-seafoam" : "text-coral")}>
          <Square tone={passed ? "ok" : "error"} />
          {passed ? `Dry run passed · ${lockWords(statements.map((s) => s.lock))}` : "The dry run didn't pass: nothing would be applied."}
        </p>
      )}
      <Footer>
        {failure?.kind === "not_applied" ? (
          <>
            <Button variant="ghost" onClick={editor.back}>
              Back to editing
            </Button>
            <Button onClick={() => void editor.reload()}>Reload privileges</Button>
          </>
        ) : failure?.kind === "stale" ? (
          <>
            <Button variant="ghost" onClick={editor.back}>
              Back
            </Button>
            <Button onClick={() => void editor.reload()}>Reload privileges</Button>
          </>
        ) : (
          <>
            <Button variant="ghost" onClick={editor.back} disabled={review.applying}>
              Back
            </Button>
            <Button disabled={!canApply} onClick={() => void editor.apply()}>
              {review.applying ? "Applying…" : `Apply ${changesWord(n)}`}
            </Button>
          </>
        )}
      </Footer>
    </>
  );
}

function mismatchText(m: Mismatch, index: PrivilegeIndex): string {
  const what = `${m.privilege}${m.column ? ` (${escapeName(m.column)})` : ""} on ${serverTargetText(m.object, index)}`;
  const who = escapeName(m.grantee);
  if (m.wanted) return `${what} wasn't granted to ${who}${m.still_granted_by ? ` (granted by ${escapeName(m.still_granted_by)})` : ""}.`;
  return m.still_granted_by
    ? `${what} is still held by ${who}: it was granted by ${escapeName(m.still_granted_by)}, so revoking it as the owner left it in place. Revoke it in SQL as ${escapeName(m.still_granted_by)}, or leave ${m.privilege} checked.`
    : `${what} is still held by ${who}.`;
}

function FailureBox({ failure, editor, role, count }: { failure: Failure; editor: PrivilegeEditorState; role: string; count: number }) {
  const box = "flex gap-2.5 border border-coral/60 bg-coral/[0.08] px-3.5 py-3 text-[13px]";
  switch (failure.kind) {
    case "stale": {
      const names = failure.objects.map((o) => serverTargetText(o, editor.index));
      const kept = count === 1 ? "your 1 change is kept" : `your ${count} changes are kept`;
      return (
        <div role="alert" className="flex gap-2.5 border border-line-strong px-3.5 py-3 text-[13px] text-chalk">
          <Square tone="warn" />
          <p>{`Privileges on ${names.length > 0 ? listText(names) : "what you changed"} changed since you opened this page. Reload to see them; ${kept} to re-apply.`}</p>
        </div>
      );
    }
    case "exposes":
      return (
        <div role="alert" className={box}>
          <Square tone="error" />
          <div className="space-y-1">
            <p className="font-semibold text-coral">Error: All rows can't be turned on for {escapeName(role)}.</p>
            {failure.chain.length > 0 ? (
              <p className="text-chalk">
                It would expose every row to the data API: <span className="font-mono">{failure.chain.map(escapeName).join(" → ")}</span>.
              </p>
            ) : (
              <p className="text-chalk">{failure.message}</p>
            )}
            <p className="text-chalk-dim">
              Remove {failure.opIndex === null ? "the all-rows change" : `change ${failure.opIndex + 1}`}
              {failure.chain.length > 1 ? `, or take ${escapeName(role)} out of ${escapeName(failure.chain[0])}'s chain first.` : "."}
            </p>
          </div>
        </div>
      );
    case "not_applied":
      return (
        <div role="alert" className={box}>
          <Square tone="error" />
          <div className="space-y-1">
            <p className="font-semibold text-coral">Error: nothing was applied.</p>
            {failure.mismatches.length === 0 ? (
              <p className="text-chalk">{failure.message}</p>
            ) : (
              <ul className="space-y-1 text-chalk">
                {failure.mismatches.map((m, i) => (
                  <li key={i}>{mismatchText(m, editor.index)}</li>
                ))}
              </ul>
            )}
            <p className="text-chalk-dim">The whole change rolled back.</p>
          </div>
        </div>
      );
    case "error":
      return (
        <div role="alert" className={box}>
          <Square tone="error" />
          <div className="space-y-1">
            <p className="font-semibold text-coral">Error: {failure.message}</p>
            <p className="text-chalk-dim">Nothing was applied; your changes are still staged.</p>
          </div>
        </div>
      );
  }
}
