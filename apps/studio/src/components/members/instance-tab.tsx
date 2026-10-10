import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  getGetInstanceSettingsQueryKey,
  useUpdateInstanceSettings,
  type InstanceSettings,
} from "@chelabase/management-client";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Switch } from "@/components/ui/switch";
import { errorText } from "@/lib/errors";
import { ACCESS_EVENTS_KEY } from "./keys";

/** The instance's own settings (instance admins): today, open sign-up. */
export function InstanceTab({ settings }: { settings: InstanceSettings }) {
  const queryClient = useQueryClient();
  const update = useUpdateInstanceSettings();
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const on = settings.open_signup;

  async function save(next: boolean) {
    setError(null);
    try {
      const saved = await update.mutateAsync({ data: { open_signup: next } });
      queryClient.setQueryData(getGetInstanceSettingsQueryKey(), saved);
      void queryClient.invalidateQueries({ queryKey: ACCESS_EVENTS_KEY });
    } catch (err) {
      setError(errorText(err));
    } finally {
      setConfirming(false);
    }
  }

  return (
    <div className="space-y-3 border-y border-line py-4">
      <div className="max-w-[65ch] space-y-1">
        <h2 className="text-sm font-medium text-chalk">Open sign-up</h2>
        <p id="open-signup-desc" className="text-sm text-chalk-dim">
          Anyone who can reach this Studio can create an account. Each new account waits under Requests until a tenant
          admin approves it.
        </p>
      </div>
      {/* The switch and its state on one row, under the description, at every width. */}
      <div className="flex flex-wrap items-center gap-3">
        {/* One label source: the switch is named by aria-label, described by the paragraph. */}
        <Switch
          aria-label="Open sign-up"
          aria-describedby="open-signup-desc"
          checked={on}
          disabled={update.isPending}
          // Turning it on lets strangers in (as requests): it asks first. Off acts at once.
          onCheckedChange={(next) => (next ? setConfirming(true) : void save(false))}
        />
        <p className={on ? "text-sm text-seafoam" : "text-sm text-chalk-dim"}>
          {update.isPending ? "Saving…" : on ? "On: new accounts wait for approval" : "Off: only invited people can join"}
        </p>
        {error && (
          <p role="alert" className="basis-full text-sm text-coral">
            {error}
          </p>
        )}
      </div>
      <ConfirmDialog
        open={confirming}
        onOpenChange={setConfirming}
        title="Turn on open sign-up?"
        body="Anyone who reaches the sign-in page can request access. Each request waits under Requests until a tenant admin approves or declines it."
        confirm="Turn on"
        busy={update.isPending}
        onConfirm={() => void save(true)}
      />
    </div>
  );
}
