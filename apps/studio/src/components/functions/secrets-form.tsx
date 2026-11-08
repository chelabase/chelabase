import { useId, useRef, useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { deleteFunctionSecret, getListFunctionSecretsQueryKey, putFunctionSecret, useListFunctionSecrets } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { functionErrorText, secretNameReason } from "@/lib/functions";
import { ReadOnlyHint, VIEWER_SECRETS_REASON } from "./read-only-hint";

export interface FunctionSecretsProps {
  databaseId: string;
  branchId: string;
  /** The function's name. */
  name: string;
  canChange: boolean;
}

const field = "h-8 border border-input bg-ink px-2 text-sm disabled:opacity-50";

/**
 * A function's secrets on the branch: their names, and a form to add or replace one. Values are
 * write-only: the value field is uncontrolled (React state never holds it), it is read once on
 * submit, sent with the plain function (never the mutation cache) and cleared after.
 */
export function FunctionSecrets({ databaseId, branchId, name, canChange }: FunctionSecretsProps) {
  const queryClient = useQueryClient();
  const list = useListFunctionSecrets(databaseId, branchId, name);
  const [secretName, setSecretName] = useState("");
  const [hasValue, setHasValue] = useState(false);
  const valueRef = useRef<HTMLInputElement>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const nameId = useId();
  const valueId = useId();
  const reasonId = useId();
  const reason = canChange ? null : VIEWER_SECRETS_REASON;
  const nameReason = secretName === "" ? null : secretNameReason(secretName);
  const ready = secretName !== "" && nameReason === null && hasValue && !busy;
  const refresh = () => queryClient.invalidateQueries({ queryKey: getListFunctionSecretsQueryKey(databaseId, branchId, name) });

  function clearValue() {
    if (valueRef.current) valueRef.current.value = "";
    setHasValue(false);
  }

  async function save(event: FormEvent) {
    event.preventDefault();
    const input = valueRef.current;
    if (!ready || !input) return;
    setBusy(true);
    setError(null);
    try {
      await putFunctionSecret(databaseId, branchId, name, secretName, { value: input.value });
      setSecretName("");
      clearValue();
      await refresh();
    } catch (err) {
      setError(functionErrorText(err));
    } finally {
      setBusy(false);
    }
  }

  async function remove(secret: string) {
    setBusy(true);
    setError(null);
    try {
      await deleteFunctionSecret(databaseId, branchId, name, secret);
      setDeleting(null);
      await refresh();
    } catch (err) {
      setError(functionErrorText(err));
    } finally {
      setBusy(false);
    }
  }

  const names = list.data ?? [];
  return (
    <div className="space-y-3 text-sm">
      {list.error ? (
        <p role="alert" className="text-coral">
          {functionErrorText(list.error)}
        </p>
      ) : !list.data ? (
        <p className="text-chalk-dim">Loading…</p>
      ) : names.length === 0 ? (
        <p className="text-chalk-dim">No secrets yet. The function reads each one from env.</p>
      ) : (
        <ul className="divide-y divide-line border border-line">
          {names.map((s) => (
            <li key={s} className="flex items-center gap-2 px-3 py-1.5">
              <span className="min-w-0 flex-1 truncate font-mono text-xs">{s}</span>
              <span aria-hidden="true" className="font-mono text-xs text-chalk-dim">
                ••••••
              </span>
              <ReadOnlyHint reason={reason}>
                <Button size="xs" variant="ghost" disabled={!canChange || busy} aria-label={`Replace ${s}`} onClick={() => {
                    setSecretName(s);
                    valueRef.current?.focus();
                  }}
                >
                  Replace
                </Button>
              </ReadOnlyHint>
              <ReadOnlyHint reason={reason}>
                <Button size="xs" variant="ghost" disabled={!canChange || busy} aria-label={`Delete ${s}`} onClick={() => setDeleting(s)}>
                  Delete
                </Button>
              </ReadOnlyHint>
            </li>
          ))}
        </ul>
      )}

      {deleting && (
        <div className="space-y-2 border border-line p-3">
          <p>
            Delete <span className="font-mono">{deleting}</span>? The function stops seeing it on its next call.
          </p>
          <div className="flex gap-2">
            <Button size="sm" variant="destructive" disabled={busy} onClick={() => void remove(deleting)}>
              Confirm delete
            </Button>
            <Button size="sm" variant="outline" onClick={() => setDeleting(null)}>
              Cancel
            </Button>
          </div>
        </div>
      )}

      <form onSubmit={save} className="flex flex-wrap items-end gap-2" autoComplete="off">
        <label htmlFor={nameId} className="flex flex-col gap-1">
          <span className="text-xs text-chalk-dim">Secret name</span>
          <input
            id={nameId}
            value={secretName}
            onChange={(e) => setSecretName(e.target.value)}
            disabled={!canChange}
            spellCheck={false}
            autoComplete="off"
            aria-invalid={nameReason !== null}
            aria-describedby={nameReason || reason ? reasonId : undefined}
            className={`${field} w-44 font-mono`}
          />
        </label>
        <label htmlFor={valueId} className="flex min-w-40 flex-1 flex-col gap-1">
          <span className="text-xs text-chalk-dim">Value</span>
          <input
            id={valueId}
            ref={valueRef}
            type="password"
            autoComplete="new-password"
            defaultValue=""
            onChange={(e) => setHasValue(e.target.value !== "")}
            disabled={!canChange}
            className={field}
          />
        </label>
        <ReadOnlyHint reason={reason}>
          <Button type="submit" size="sm" disabled={!ready}>
            Save secret
          </Button>
        </ReadOnlyHint>
      </form>
      {(nameReason || reason) && (
        <p id={reasonId} className="text-xs text-chalk-dim">
          {nameReason ?? reason}
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
