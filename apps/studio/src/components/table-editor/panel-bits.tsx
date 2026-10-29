import type { DryRunState } from "./use-dry-run";
import { useId, type ComponentProps } from "react";
import type { LockLevel } from "@chelabase/management-client";
import { Check, PanelRightClose, PanelRightOpen, type LucideIcon } from "lucide-react";
import { lockText, skipText, type EditStatus, type StatementView } from "@/lib/table-editor/dry-run";
import { cn } from "@/lib/utils";

export const inputClass =
  "h-9 w-full border border-input bg-ink px-2 text-sm text-chalk outline-none placeholder:text-chalk-dim focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand disabled:opacity-60 aria-[invalid=true]:border-coral";

/**
 * The editor's square checkbox: an ink box with a chalk hairline, a sand edge and a drawn sand check
 * when checked (a sand bar when indeterminate). A native input, so labels, keys and forms work as usual.
 */
export function Checkbox({ className, ref, ...props }: Omit<ComponentProps<"input">, "type">) {
  return (
    <span className={cn("relative inline-flex size-4 shrink-0", className)}>
      <input
        ref={ref}
        type="checkbox"
        {...props}
        className="peer size-4 shrink-0 cursor-default appearance-none border border-chalk/40 bg-ink outline-none checked:border-sand indeterminate:border-sand focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-sand disabled:opacity-50"
      />
      <Check aria-hidden="true" size={12} strokeWidth={2.5} className="pointer-events-none absolute inset-0 m-auto hidden text-sand peer-checked:block peer-indeterminate:hidden" />
      <span aria-hidden="true" className="pointer-events-none absolute inset-0 m-auto hidden h-0.5 w-2 bg-sand peer-indeterminate:block" />
    </span>
  );
}

/** A labelled square checkbox (sand tick). */
export function CheckField({ label, checked, onChange, disabled, note }: { label: string; checked: boolean; onChange: (v: boolean) => void; disabled?: boolean; note?: string }) {
  return (
    <label className={cn("flex h-8 items-center gap-2 text-sm", disabled ? "text-chalk-dim" : "text-chalk")}>
      <Checkbox checked={checked} disabled={disabled} onChange={(e) => onChange(e.target.checked)} />
      <span>{label}</span>
      {note && <span className="text-xs text-chalk-dim">{note}</span>}
    </label>
  );
}

/** Every right panel's collapse control: one icon, size and box. */
export function CollapseButton({ onClick, className }: { onClick: () => void; className?: string }) {
  return (
    <button
      type="button"
      aria-label="Collapse panel"
      onClick={onClick}
      className={cn("flex size-7 shrink-0 items-center justify-center border border-line text-chalk-dim outline-none hover:text-chalk focus-visible:outline-2 focus-visible:outline-sand", className)}
    >
      <PanelRightClose aria-hidden="true" size={16} strokeWidth={1.5} />
    </button>
  );
}

/**
 * A collapsed panel's 48px strip (an activity-bar slot): the expand control, then one icon naming what the panel
 * holds, with a small badge (a count, or a sand dot for a held draft). The full name is the button's tooltip and its
 * accessible description; the icon expands too, for the pointer, but is no second tab stop.
 */
export function CollapsedStrip({ text, icon: Icon, badge, onExpand }: { text: string; icon: LucideIcon; badge?: number | "dot"; onExpand: () => void }) {
  const stripId = useId();
  return (
    <div className="flex w-12 shrink-0 flex-col items-center gap-3 border-l border-line-strong bg-deep py-3">
      <button
        type="button"
        aria-label="Expand panel"
        aria-describedby={stripId}
        title={text}
        onClick={onExpand}
        className="flex size-9 items-center justify-center border border-input text-chalk outline-none hover:border-sand focus-visible:outline-2 focus-visible:outline-sand"
      >
        <PanelRightOpen aria-hidden="true" size={16} strokeWidth={1.5} />
      </button>
      <span id={stripId} className="sr-only">
        {text}
      </span>
      <span aria-hidden="true" data-testid="strip-icon" title={text} onClick={onExpand} className="relative flex size-9 cursor-pointer items-center justify-center text-chalk-dim hover:text-chalk">
        <Icon size={18} strokeWidth={1.5} />
        {badge === "dot" && <span data-testid="strip-badge" className="absolute top-1.5 right-1.5 size-1.5 bg-sand" />}
        {typeof badge === "number" && (
          <span data-testid="strip-badge" className="absolute -top-0.5 -right-0.5 min-w-4 bg-sand px-1 text-center font-sans text-[11px] leading-4 text-ink tabular-nums">
            {badge}
          </span>
        )}
      </span>
    </div>
  );
}

/** The lock words in a passed dry run's line: "ACCESS EXCLUSIVE lock", "no table lock". */
export function lockWords(locks: readonly LockLevel[]): string {
  const strongest = LOCK_ORDER.find((l) => locks.includes(l)) ?? "none";
  return strongest === "none" ? "no table lock" : `${strongest.replace(/_/g, " ").toUpperCase()} lock`;
}
const LOCK_ORDER: readonly LockLevel[] = ["access_exclusive", "share_row_exclusive", "share", "share_update_exclusive", "none"];

/** A passed dry run in one line: "Dry run passed · ACCESS EXCLUSIVE lock · no rewrite". */
export function passedText(statements: readonly StatementView[]): string {
  return `Dry run passed · ${lockWords(statements.map((s) => s.lock))} · ${statements.some((s) => s.rewrites) ? "rewrites the table" : "no rewrite"}`;
}

export const changesWord = (n: number) => `${n} ${n === 1 ? "change" : "changes"}`;

/** Statements as one SQL block, a line each (the highlighted edit's in sand). */
export function SqlStatements({ statements, pending, highlight }: { statements: StatementView[]; pending: boolean; highlight?: number | null }) {
  if (statements.length === 0) {
    return <p className="text-[13px] text-chalk-dim">{pending ? "Running the dry run…" : "No SQL yet."}</p>;
  }
  return (
    <div className="max-h-64 overflow-auto border border-line bg-ink px-3 py-2 font-mono text-xs leading-5 text-chalk">
      {statements.map((s, i) => (
        <code
          key={i}
          className={cn("block break-words whitespace-pre-wrap", highlight != null && s.edit === highlight && "text-sand", s.skipped && "text-chalk-dim")}
        >
          {s.sql}
        </code>
      ))}
    </div>
  );
}

export function Dot({ tone }: { tone: "ok" | "error" | "wait" | "warn" }) {
  return (
    <span
      aria-hidden="true"
      className={cn("shrink-0", tone === "wait" ? "mt-[8px] h-0.5 w-2 bg-chalk-dim" : "mt-[5px] size-2", tone === "ok" ? "bg-seafoam" : tone === "error" ? "bg-coral" : tone === "warn" && "border border-coral")}
    />
  );
}

export function ErrorText({ status }: { status: Extract<EditStatus, { kind: "error" }> }) {
  return (
    <div className="space-y-0.5 text-[13px]">
      <p className="text-coral">Error: {status.message}</p>
      {status.detail && <p className="text-chalk-dim">{status.detail}</p>}
      {status.hint && <p className="text-chalk-dim">{status.hint}</p>}
    </div>
  );
}

/** One edit's dry-run outcome in a line (the column form's preview). */
export function DryRunSummary({ state, edit }: { state: DryRunState; edit: number }) {
  const { view, failure, running } = state;
  if (running && !view && !failure) {
    return (
      <p className="flex gap-2 text-[13px] text-chalk-dim">
        <Dot tone="wait" />
        Checking with a dry run…
      </p>
    );
  }
  if (failure) {
    if (failure.status.kind !== "error") return null;
    return (
      <div className="flex gap-2">
        <Dot tone="error" />
        <div>
          {failure.edit !== null && failure.edit !== edit && <p className="text-[13px] text-chalk-dim">An earlier pending change fails:</p>}
          <ErrorText status={failure.status} />
        </div>
      </div>
    );
  }
  if (!view) return null;
  const mine = view.statements.filter((s) => s.edit === edit);
  const status = view.perEdit[edit];
  const locks = [...new Set(mine.map((s) => lockText(s.lock)))];
  return (
    <div className={cn("space-y-1 text-[13px]", running && "opacity-60")}>
      {status?.kind === "rendered_only" ? (
        <p className="flex gap-2 text-chalk-dim">
          <Dot tone="warn" />
          {skipText(status.reason)}
        </p>
      ) : (
        <p className="flex gap-2 text-chalk-dim">
          <Dot tone="ok" />
          {passedText(mine)}
        </p>
      )}
      {/* A passed dry run names its lock in its line; a change that wasn't simulated says what it will take. */}
      {status?.kind === "rendered_only" &&
        locks.map((l) => (
          <p key={l} className="pl-4 text-chalk-dim">
            {l}
          </p>
        ))}
    </div>
  );
}

