import { useState, type ReactNode } from "react";
import { Dialog as DialogPrimitive } from "@base-ui/react/dialog";
import { X } from "lucide-react";
import { useListFunctionDeployments, type Function as FunctionInfo } from "@chelabase/management-client";
import { CopyButton } from "@/components/copy-button";
import { Button } from "@/components/ui/button";
import { curlFor, functionErrorText, invokeUrl } from "@/lib/functions";
import { relativeTime, utc } from "@/lib/members";
import { ReadOnlyHint, VIEWER_REASON } from "./read-only-hint";
import { RunAsForm } from "./run-as-form";
import { FunctionSecrets } from "./secrets-form";

export interface FunctionDrawerProps {
  fn: FunctionInfo;
  databaseId: string;
  branchId: string;
  apiUrl: string;
  canChange: boolean;
  /** Sends a PATCH for this function; rejects with the API's refusal. */
  update: (body: { active_deployment_id?: string; run_as_role?: string | null }) => Promise<void>;
  onClose: () => void;
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="space-y-2 border-t border-line pt-4">
      <h3 className="text-sm font-semibold">{title}</h3>
      {children}
    </section>
  );
}

const AUTH_TEXT: Record<string, string> = {
  key: "Any data key (public or secret).",
  app_user: "A signed-in app user, or a secret key for server calls.",
  public: "No credential: anyone with the URL.",
};

/** A side panel with one function: how to call it, its run-as role, its deployments and its secrets. */
export function FunctionDrawer({ fn, databaseId, branchId, apiUrl, canChange, update, onClose }: FunctionDrawerProps) {
  const deployments = useListFunctionDeployments(databaseId, branchId, fn.name);
  const [activating, setActivating] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const url = invokeUrl(apiUrl, fn.auth, branchId, fn.name);
  const curl = curlFor(apiUrl, fn.auth, branchId, fn.name);
  const reason = canChange ? null : VIEWER_REASON;

  async function makeActive(id: string) {
    setActivating(id);
    setError(null);
    try {
      await update({ active_deployment_id: id });
      await deployments.refetch();
    } catch (err) {
      setError(functionErrorText(err));
    } finally {
      setActivating(null);
    }
  }

  return (
    <DialogPrimitive.Root open onOpenChange={(open) => !open && onClose()}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Backdrop className="fixed inset-0 bg-black/40" />
        <DialogPrimitive.Popup className="fixed top-0 right-0 h-full w-[34rem] max-w-full space-y-4 overflow-y-auto border-l border-line-strong bg-background p-6 text-foreground shadow-lg">
          <div className="flex items-start justify-between gap-2">
            <div className="min-w-0 space-y-1">
              <DialogPrimitive.Title className="font-mono text-lg font-semibold break-all">{fn.name}</DialogPrimitive.Title>
              <DialogPrimitive.Description className="text-xs text-chalk-dim">
                {fn.kind} · {fn.enabled ? "enabled" : "disabled"} on this branch
              </DialogPrimitive.Description>
            </div>
            <DialogPrimitive.Close render={<Button size="icon-sm" variant="ghost" aria-label="Close" />}>
              <X aria-hidden="true" size={16} strokeWidth={1.5} />
            </DialogPrimitive.Close>
          </div>

          {fn.last_error && (
            <p className="border border-coral/50 p-2 font-mono text-xs break-all text-coral">
              <span className="font-sans font-semibold">Last error: </span>
              {fn.last_error}
            </p>
          )}

          <Section title="Invoke">
            <p className="text-xs text-chalk-dim">{fn.auth ? AUTH_TEXT[fn.auth] : "Auth isn't set: calls are refused until it is."}</p>
            <div className="flex items-center gap-2">
              <code className="min-w-0 flex-1 bg-ink px-2 py-1 font-mono text-xs break-all">{url}</code>
              <CopyButton value={url} label="Copy invoke URL" />
            </div>
            <div className="flex items-start gap-2">
              <pre className="min-w-0 flex-1 overflow-x-auto bg-ink px-2 py-1 font-mono text-xs">{curl}</pre>
              <CopyButton value={curl} label="Copy curl command" />
            </div>
          </Section>

          <Section title="Run as">
            <RunAsForm key={fn.run_as_role ?? ""} fn={fn} canChange={canChange} onSave={(role) => update({ run_as_role: role })} />
          </Section>

          <Section title="Deployments">
            {deployments.error ? (
              <p role="alert" className="text-sm text-coral">
                {functionErrorText(deployments.error)}
              </p>
            ) : !deployments.data ? (
              <p className="text-sm text-chalk-dim">Loading…</p>
            ) : deployments.data.length === 0 ? (
              <p className="text-sm text-chalk-dim">No deployments yet.</p>
            ) : (
              <ul className="divide-y divide-line border border-line text-sm">
                {deployments.data.map((d) => {
                  const active = d.id === fn.deployment_id;
                  return (
                    <li key={d.id} className="flex items-center gap-3 px-3 py-2">
                      <div className="min-w-0 flex-1">
                        <p className="flex items-center gap-2">
                          <span className="font-mono text-xs">{d.id.slice(0, 8)}</span>
                          {active && <span className="border border-seafoam/60 px-1.5 text-xs text-seafoam">Active</span>}
                        </p>
                        <p className="truncate text-xs text-chalk-dim">
                          {d.note ? `${d.note} · ` : ""}
                          <time dateTime={d.created_at} title={utc(d.created_at)}>
                            {relativeTime(d.created_at)}
                          </time>
                        </p>
                      </div>
                      {!active && (
                        <ReadOnlyHint reason={reason}>
                          <Button size="xs" variant="outline" disabled={!canChange || activating !== null} aria-label={`Make ${d.id.slice(0, 8)} active`} onClick={() => void makeActive(d.id)}>
                            {activating === d.id ? "Activating…" : "Make active"}
                          </Button>
                        </ReadOnlyHint>
                      )}
                    </li>
                  );
                })}
              </ul>
            )}
            {error && (
              <p role="alert" className="text-sm text-coral">
                {error}
              </p>
            )}
          </Section>

          <Section title="Secrets">
            <FunctionSecrets databaseId={databaseId} branchId={branchId} name={fn.name} canChange={canChange} />
          </Section>
        </DialogPrimitive.Popup>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}
