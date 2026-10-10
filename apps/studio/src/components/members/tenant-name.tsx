import { useEffect, useId, useRef, useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useUpdateTenant, type Tenant } from "@chelabase/management-client";
import { refreshAccess } from "./keys";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";

export const tenantLabel = (t: Pick<Tenant, "display_name" | "slug">) => t.display_name || t.slug;

/**
 * The page's description line, naming the tenant (R36). Tenant admins can rename it in place:
 * an empty name clears it and the slug shows again.
 */
export function TenantName({ tenant, editable, onStatus }: { tenant: Tenant; editable: boolean; onStatus: (m: string) => void }) {
  const id = useId();
  const queryClient = useQueryClient();
  const update = useUpdateTenant();
  const [editing, setEditing] = useState(false);
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);
  /** Rename: focus returns here when the form closes (saved or cancelled), not to the page body. */
  const renameButton = useRef<HTMLButtonElement>(null);
  const [refocus, setRefocus] = useState(false);
  useEffect(() => {
    if (refocus && !editing) {
      renameButton.current?.focus();
      setRefocus(false);
    }
  }, [refocus, editing]);
  const close = () => {
    setEditing(false);
    setRefocus(true);
  };

  async function save(event: FormEvent) {
    event.preventDefault();
    setError(null);
    const name = value.trim();
    try {
      const saved = await update.mutateAsync({ data: { display_name: name === "" ? null : name } });
      refreshAccess(queryClient);
      close();
      onStatus(saved.display_name ? `Renamed the tenant to ${saved.display_name}.` : `Cleared the tenant's name; it shows as ${tenant.slug}.`);
    } catch (err) {
      setError(errorText(err));
    }
  }

  if (!editing) {
    return (
      <div className="flex flex-wrap items-center gap-2">
        <p className="text-sm text-chalk-dim">People in {tenantLabel(tenant)} and what they can reach.</p>
        {editable && (
          <Button
            ref={renameButton}
            variant="ghost"
            size="xs"
            onClick={() => {
              setValue(tenant.display_name ?? "");
              setError(null);
              setEditing(true);
            }}
          >
            Rename
          </Button>
        )}
      </div>
    );
  }
  return (
    <form onSubmit={save} className="space-y-1">
      <label htmlFor={id} className="text-sm font-medium">
        Tenant name
      </label>
      <div className="flex flex-wrap items-center gap-2">
        <input
          id={id}
          autoFocus
          maxLength={80}
          value={value}
          placeholder={tenant.slug}
          aria-invalid={error ? true : undefined}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={(e) => {
            // Escape cancels, like the Cancel button.
            if (e.key === "Escape" && !update.isPending) {
              e.preventDefault();
              close();
            }
          }}
          className="h-8 w-72 border border-input bg-ink px-2 text-sm text-chalk outline-none placeholder:text-chalk-dim focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand"
        />
        <Button type="submit" size="sm" disabled={update.isPending}>
          {update.isPending ? "Saving…" : "Save"}
        </Button>
        <Button type="button" size="sm" variant="ghost" disabled={update.isPending} onClick={close}>
          Cancel
        </Button>
      </div>
      <p className="text-xs text-chalk-dim">Leave it empty to show {tenant.slug}.</p>
      {error && (
        <p role="alert" className="text-sm text-coral">
          {error}
        </p>
      )}
    </form>
  );
}
