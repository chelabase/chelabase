import { Select } from "@/components/ui/select";
import { useId, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ApiError,
  deleteRole,
  getPreviewRoleDeleteQueryKey,
  previewRoleDelete,
  type BranchRole,
  type DeletedRole,
  type RoleDeletePreview,
  type RoleExposure,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { Checkbox } from "@/components/table-editor/panel-bits";
import { errorText } from "@/lib/errors";
import { escapeName } from "../hub/escape-name";
import { plural } from "../member-page/logins";
import { isManaged } from "./model";

/** The powerless role a deleted role's objects go to by default (T10). */
export const QUARANTINE = "chelabase_orphaned";

const KIND_TEXT: Record<RoleExposure["kind"], string> = { function: "function", procedure: "procedure", view: "view", materialized_view: "materialized view" };

/** Roles that may take a deleted role's objects: managed, not owner-level, not the role itself. */
export function successorChoices(rows: readonly BranchRole[], role: string): BranchRole[] {
  return rows.filter((r) => isManaged(r) && r.level !== "owner_level" && r.name !== role);
}

const isKey = (k: unknown): k is { id: string; name: string } =>
  typeof k === "object" && k !== null && typeof (k as { id?: unknown }).id === "string" && typeof (k as { name?: unknown }).name === "string";
const exposuresOf = (v: unknown): RoleExposure[] =>
  Array.isArray(v) ? v.filter((x): x is RoleExposure => typeof x === "object" && x !== null && typeof (x as RoleExposure).identity === "string") : [];

/** The Danger zone: what deleting the role does, then the dialog. */
export function DangerZone(props: Omit<DeleteRoleDialogProps, "open" | "onOpenChange">) {
  const [open, setOpen] = useState(false);
  return (
    <div className="space-y-3 border border-coral/60 p-4">
      <h2 className="text-base font-semibold">Delete this role</h2>
      <p className="text-sm text-chalk-dim">
        Deleting {escapeName(props.role)} closes its sessions on this branch; the objects it owns go to the successor you pick, or to quarantine, where no one can use them. Data
        API keys that use it must be revoked first.
      </p>
      <Button variant="destructive" onClick={() => setOpen(true)}>
        Delete role…
      </Button>
      {open && <DeleteRoleDialog {...props} open={open} onOpenChange={setOpen} />}
    </div>
  );
}

export interface DeleteRoleDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  databaseId: string;
  branchId: string;
  role: string;
  roles: readonly BranchRole[];
  /** After "Done": back to the Roles tab. */
  onDeleted: () => void;
}

/**
 * Delete a role with a successor (T10): the preview names what a successor would expose (routines
 * and views that would run with its rights for others), and a named successor that exposes anything
 * needs "I understand" before the delete, sent with the preview's `confirm` token. A fresh list on
 * 409 `exposure_unconfirmed` asks again.
 */
export function DeleteRoleDialog({ open, onOpenChange, databaseId, branchId, role, roles, onDeleted }: DeleteRoleDialogProps) {
  const queryClient = useQueryClient();
  const selectId = useId();
  const [successor, setSuccessor] = useState(QUARANTINE);
  const [confirmed, setConfirmed] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<{ message: string; keys: { id: string; name: string }[] } | null>(null);
  const [done, setDone] = useState<DeletedRole | null>(null);
  const params = { successor };
  const previewKey = getPreviewRoleDeleteQueryKey(databaseId, branchId, role, params);
  const preview = useQuery({
    queryKey: previewKey,
    queryFn: ({ signal }) => previewRoleDelete(databaseId, branchId, role, params, { signal }),
    enabled: open && done === null,
    staleTime: 0,
    gcTime: 0,
  });
  const token = preview.data?.confirm ?? null;
  const needsConfirm = token !== null;
  const ready = preview.data !== undefined && !preview.isFetching && (!needsConfirm || confirmed === token) && !busy;
  const choices = successorChoices(roles, role);

  async function remove() {
    if (!ready || !preview.data) return;
    setBusy(true);
    setError(null);
    try {
      const result = await deleteRole(databaseId, branchId, role, { successor, ...(token !== null ? { confirm: token } : {}) });
      setDone(result);
    } catch (err) {
      if (err instanceof ApiError && err.code === "exposure_unconfirmed") {
        // Something changed since the preview: show the fresh list and ask again.
        const fresh: RoleDeletePreview = { successor, exposures: exposuresOf(err.extra.exposures), confirm: typeof err.extra.confirm === "string" ? err.extra.confirm : null };
        queryClient.setQueryData(previewKey, fresh);
        setConfirmed(null);
      } else {
        const keys = err instanceof ApiError && err.code === "role_in_use" && Array.isArray(err.extra.keys) ? err.extra.keys.filter(isKey) : [];
        setError({ message: errorText(err), keys });
      }
    } finally {
      setBusy(false);
    }
  }

  const close = (next: boolean) => {
    if (next) return;
    if (done) onDeleted();
    onOpenChange(false);
  };

  if (done) {
    return (
      <Modal open={open} onOpenChange={close} title={`Deleted ${escapeName(role)}`} size="medium">
        <div className="space-y-3 text-sm">
          <p>
            {plural(done.reassigned.count, "object")} went to <span className="font-mono">{escapeName(done.successor)}</span>.
          </p>
          {done.reassigned.definers.length > 0 && (
            <div className="space-y-1.5">
              <p className="text-chalk-dim">Review these SECURITY DEFINER routines: they now run with the successor's rights. Their EXECUTE for PUBLIC was revoked.</p>
              <ul className="space-y-0.5 border-l border-coral/60 pl-3 font-mono text-xs">
                {done.reassigned.definers.map((d) => (
                  <li key={d}>{escapeName(d)}</li>
                ))}
              </ul>
            </div>
          )}
          <div className="flex justify-end">
            <Button onClick={() => close(false)}>Done</Button>
          </div>
        </div>
      </Modal>
    );
  }

  const exposures = preview.data?.exposures ?? [];
  return (
    <Modal open={open} onOpenChange={close} title={`Delete role ${escapeName(role)}`} description="Its sessions on this branch are closed and its memberships dropped." size="medium">
      <div className="space-y-4 text-sm">
        <div className="space-y-1.5">
          <label htmlFor={selectId} className="text-sm font-medium">
            Successor
          </label>
          <Select
            id={selectId}
            value={successor}
            disabled={busy}
            onChange={(e) => {
              setSuccessor(e.target.value);
              setConfirmed(null);
              setError(null);
            }}
            className="h-9 font-mono"
          >
            <option value={QUARANTINE}>Quarantine (no one)</option>
            {choices.map((r) => (
              <option key={r.name} value={r.name}>
                {escapeName(r.name)}
              </option>
            ))}
          </Select>
          <p className="text-xs text-chalk-dim">
            {successor === QUARANTINE ? "The objects stay, owned by a role no one can use, until an Admin hands them on." : `The objects go to ${escapeName(successor)}, with its rights.`}
          </p>
        </div>

        {preview.isError ? (
          <p role="alert" className="text-coral">
            {errorText(preview.error)}
          </p>
        ) : preview.isFetching && !preview.data ? (
          <p className="text-chalk-dim">Checking what the successor would expose…</p>
        ) : exposures.length > 0 ? (
          <div className="space-y-2">
            <p className={needsConfirm ? "text-chalk" : "text-chalk-dim"}>
              {needsConfirm
                ? `These would run with ${escapeName(successor)}'s rights for the roles listed:`
                : "These are in use by other roles; in quarantine they stop working for them:"}
            </p>
            <ul className="divide-y divide-line border-y border-line">
              {exposures.map((x) => (
                <li key={`${x.database} ${x.kind} ${x.identity}`} className="grid grid-cols-[minmax(0,1fr)_auto] gap-x-3 py-1.5">
                  <span className="font-mono text-xs break-all text-chalk">{escapeName(x.identity)}</span>
                  <span className="text-xs text-chalk-dim">
                    {KIND_TEXT[x.kind] ?? x.kind} in {escapeName(x.database)}
                  </span>
                  <span className="col-span-2 text-xs text-chalk-dim">Used by {x.grantees.map(escapeName).join(", ")}</span>
                </li>
              ))}
            </ul>
            {needsConfirm && (
              <label className="flex items-start gap-2 text-sm">
                <Checkbox className="mt-0.5" checked={confirmed === token} onChange={(e) => setConfirmed(e.target.checked ? token : null)} />
                <span>I understand these will run with {escapeName(successor)}'s rights.</span>
              </label>
            )}
          </div>
        ) : null}

        {error && (
          <div role="alert" className="space-y-1 text-coral">
            <p>{error.message}</p>
            {error.keys.length > 0 && (
              <div className="space-y-1 text-chalk-dim">
                <p>Data API keys using it:</p>
                <ul className="list-disc pl-5">
                  {error.keys.map((k) => (
                    <li key={k.id}>{escapeName(k.name)}</li>
                  ))}
                </ul>
                <p>Revoke or change them in the Data API tab first.</p>
              </div>
            )}
          </div>
        )}

        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={() => close(false)}>
            Cancel
          </Button>
          <Button variant="destructive" disabled={!ready} onClick={() => void remove()}>
            {busy ? "Deleting…" : "Delete role"}
          </Button>
        </div>
      </div>
    </Modal>
  );
}
