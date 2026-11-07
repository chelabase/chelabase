import { useId, useState, type FormEvent } from "react";
import type { Function as FunctionInfo } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { functionErrorText, runAsReason } from "@/lib/functions";
import { ReadOnlyHint, VIEWER_REASON } from "./read-only-hint";

export interface RunAsFormProps {
  fn: FunctionInfo;
  canChange: boolean;
  /** Sends `run_as_role` (null clears it); rejects with the API's refusal. */
  onSave: (role: string | null) => Promise<void>;
}

const field = "h-8 border border-input bg-ink px-2 font-mono text-sm disabled:opacity-50";

/**
 * The role the function acts as itself (its `CHELABASE_SERVICE_KEY`): the current one, whether a
 * live key backs it, and setting or clearing it. A refusal (403 for a BYPASSRLS role without
 * Admin, 422 for a role that doesn't exist) is shown as the API says it.
 */
export function RunAsForm({ fn, canChange, onSave }: RunAsFormProps) {
  const [role, setRole] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputId = useId();
  const reasonId = useId();
  const reason = role === "" ? null : runAsReason(role);
  const viewer = canChange ? null : VIEWER_REASON;

  async function send(value: string | null) {
    setBusy(true);
    setError(null);
    try {
      await onSave(value);
      setRole("");
    } catch (err) {
      setError(functionErrorText(err));
    } finally {
      setBusy(false);
    }
  }

  function submit(event: FormEvent) {
    event.preventDefault();
    if (role === "" || reason || busy) return;
    void send(role.trim());
  }

  return (
    <div className="space-y-3 text-sm">
      {fn.run_as_role === null ? (
        <p className="text-chalk-dim">None. The function gets no service key and acts only as its caller.</p>
      ) : (
        <p className="flex flex-wrap items-center gap-2">
          <span className="font-mono">{fn.run_as_role}</span>
          {fn.run_as_active ? (
            <span className="border border-seafoam/60 px-1.5 text-xs text-seafoam">Live key</span>
          ) : (
            <span className="border border-coral/60 px-1.5 text-xs text-coral">No live key</span>
          )}
        </p>
      )}
      {fn.run_as_role !== null && !fn.run_as_active && (
        <p className="text-xs text-chalk-dim">Its key was revoked or never made: set the role again to make a new one.</p>
      )}
      <form onSubmit={submit} className="flex flex-wrap items-end gap-2">
        <label htmlFor={inputId} className="flex flex-col gap-1">
          <span className="text-xs text-chalk-dim">Run as role</span>
          <input
            id={inputId}
            value={role}
            onChange={(e) => setRole(e.target.value)}
            disabled={!canChange}
            spellCheck={false}
            autoComplete="off"
            placeholder={fn.run_as_role ?? "role name"}
            aria-invalid={reason !== null}
            aria-describedby={reason ? reasonId : undefined}
            className={`${field} w-48`}
          />
        </label>
        <ReadOnlyHint reason={viewer}>
          <Button type="submit" size="sm" disabled={!canChange || busy || role === "" || reason !== null}>
            {fn.run_as_role === null ? "Set role" : "Change role"}
          </Button>
        </ReadOnlyHint>
        {fn.run_as_role !== null && (
          <ReadOnlyHint reason={viewer}>
            <Button type="button" size="sm" variant="outline" disabled={!canChange || busy} onClick={() => void send(null)}>
              Clear
            </Button>
          </ReadOnlyHint>
        )}
      </form>
      {reason && (
        <p id={reasonId} className="text-xs text-chalk-dim">
          {reason}
        </p>
      )}
      {error && (
        <p role="alert" className="text-coral">
          {error}
        </p>
      )}
    </div>
  );
}
