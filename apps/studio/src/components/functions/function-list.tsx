import { useEffect, useReducer, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  getListFunctionDeploymentsQueryKey,
  getListFunctionsQueryKey,
  updateFunction,
  useListFunctionDeployments,
  useListFunctions,
  type Function as FunctionInfo,
  type UpdateFunctionRequest,
} from "@chelabase/management-client";
import { CopyIconButton } from "@/components/copy-button";
import { Switch } from "@/components/ui/switch";
import { useProjectCaps } from "@/lib/capabilities";
import { effectiveEnabled, functionErrorText, initialToggles, isPlatformModeRefusal, rolloutPending, togglesReducer } from "@/lib/functions";
import { pollWhile } from "@/lib/query";
import { serviceSpecOf, serviceStatus, type StatusTone } from "@/lib/functions-services";
import { relativeTime, utc } from "@/lib/members";
import { FunctionDrawer } from "./function-drawer";
import { ReadOnlyHint, VIEWER_REASON } from "./read-only-hint";

export interface FunctionListProps {
  databaseId: string;
  branchId: string;
  apiUrl: string;
}

export const DEV_COMMAND = "chelabase functions dev --watch";

const AUTH_LABEL: Record<string, string> = { key: "Data key", app_user: "App user", public: "Public" };

/** The function's active deployment (the list only names its id); the same query the drawer reads. */
function useActiveDeployment(databaseId: string, branchId: string, fn: FunctionInfo) {
  const deployments = useListFunctionDeployments(databaseId, branchId, fn.name, { query: { enabled: fn.deployment_id !== null } });
  return fn.deployment_id === null ? undefined : deployments.data?.find((x) => x.id === fn.deployment_id);
}

/** The kind badge, and a service's backend (from its active deployment's spec). */
function KindCell({ databaseId, branchId, fn }: { databaseId: string; branchId: string; fn: FunctionInfo }) {
  const d = useActiveDeployment(databaseId, branchId, fn);
  const backend = fn.kind === "service" && d ? serviceSpecOf(d.spec)?.backend : undefined;
  return (
    <span className="flex flex-col items-start gap-0.5">
      <span className={`border px-1.5 text-xs ${fn.kind === "service" ? "border-sand/60 text-sand" : "border-line-strong text-chalk-dim"}`}>{fn.kind}</span>
      {backend && <span className="text-xs text-chalk-dim">{backend}</span>}
    </span>
  );
}

const TONE: Record<StatusTone, string> = { good: "text-seafoam", bad: "text-coral", dim: "text-chalk-dim" };

/** The active deployment's id, note and age (the list only names its id). */
function ActiveDeployment({ databaseId, branchId, fn }: { databaseId: string; branchId: string; fn: FunctionInfo }) {
  const d = useActiveDeployment(databaseId, branchId, fn);
  if (fn.deployment_id === null) return <span className="text-chalk-dim">None</span>;
  return (
    <span className="flex min-w-0 flex-col">
      <span className="font-mono text-xs">{fn.deployment_id.slice(0, 8)}</span>
      {d && (
        <span className="truncate text-xs text-chalk-dim">
          {d.note ? `${d.note} · ` : ""}
          <time dateTime={d.created_at} title={utc(d.created_at)}>
            {relativeTime(d.created_at)}
          </time>
        </span>
      )}
    </span>
  );
}

/**
 * One branch's functions: the enable switch (optimistic, reverting with the reason on a refusal),
 * kind, active deployment, auth and last error, and a drawer per function. Mounted per branch, and
 * its toggle state is scoped to the branch as well, so nothing carries over a branch switch.
 */
export function FunctionList({ databaseId, branchId, apiUrl }: FunctionListProps) {
  const queryClient = useQueryClient();
  const canChange = useProjectCaps().atLeast("developer");
  // m11: a container service's rollover runs in the background; poll until it settles.
  const list = useListFunctions(databaseId, branchId, { query: { refetchInterval: pollWhile<FunctionInfo[]>(rolloutPending) } });
  const scope = `${databaseId}/${branchId}`;
  const [toggles, dispatch] = useReducer(togglesReducer, scope, initialToggles);
  const [open, setOpen] = useState<string | null>(null);
  useEffect(() => dispatch({ type: "scope", scope }), [scope]);
  useEffect(() => setOpen(null), [scope]);

  const refresh = () => queryClient.invalidateQueries({ queryKey: getListFunctionsQueryKey(databaseId, branchId) });

  async function flip(fn: FunctionInfo, enabled: boolean) {
    dispatch({ type: "flip", scope, name: fn.name, enabled });
    try {
      await updateFunction(databaseId, branchId, fn.name, { enabled });
      await refresh();
      dispatch({ type: "settled", scope, name: fn.name });
    } catch (err) {
      dispatch({ type: "failed", scope, name: fn.name, error: functionErrorText(err) });
    }
  }

  async function update(name: string, body: UpdateFunctionRequest) {
    await updateFunction(databaseId, branchId, name, body);
    await Promise.all([refresh(), queryClient.invalidateQueries({ queryKey: getListFunctionDeploymentsQueryKey(databaseId, branchId, name) })]);
  }

  if (list.error) {
    return (
      <p role="alert" className="p-6 text-sm text-coral">
        {isPlatformModeRefusal(list.error) ? "Edge functions aren't available on this instance." : functionErrorText(list.error)}
      </p>
    );
  }
  if (!list.data) {
    return (
      <p role="status" className="p-6 text-sm text-chalk-dim">
        Loading…
      </p>
    );
  }
  if (list.data.length === 0) {
    return (
      <div className="max-w-xl space-y-3 p-6 text-sm sm:p-8">
        <p className="font-semibold">No functions on this database yet</p>
        <p className="text-chalk-dim">
          A function is TypeScript that runs next to your data, called over HTTP with a data key. Write one in your project&apos;s{" "}
          <span className="font-mono">functions/</span> folder and run this to deploy it to the branch on every save:
        </p>
        <div className="flex w-fit max-w-full">
          <code className="flex h-9 min-w-0 items-center overflow-x-auto border border-r-0 border-input bg-ink px-3 font-mono text-xs whitespace-nowrap">{DEV_COMMAND}</code>
          <CopyIconButton value={DEV_COMMAND} label="Copy command" />
        </div>
      </div>
    );
  }

  const selected = open ? list.data.find((f) => f.name === open) : undefined;
  return (
    <div className="mx-auto w-full max-w-6xl p-4 text-sm sm:p-8">
      <div className="overflow-x-auto border border-line">
        <table className="w-full min-w-[56rem] text-left">
          <thead className="border-b border-line text-xs text-chalk-dim">
            <tr>
              <th className="px-3 py-2 font-medium">Name</th>
              <th className="px-3 py-2 font-medium">Enabled</th>
              <th className="px-3 py-2 font-medium">Kind</th>
              <th className="px-3 py-2 font-medium">Status</th>
              <th className="px-3 py-2 font-medium">Active deployment</th>
              <th className="px-3 py-2 font-medium">Auth</th>
              <th className="px-3 py-2 font-medium">Last error</th>
            </tr>
          </thead>
          <tbody>
            {list.data.map((fn) => {
              const enabled = effectiveEnabled(toggles, fn.name, fn.enabled);
              const pending = fn.name in toggles.overrides;
              const error = toggles.errors[fn.name];
              const status = serviceStatus(fn, enabled);
              return (
                <tr key={fn.id} className="border-b border-line align-top last:border-0">
                  <td className="px-3 py-2">
                    <button
                      type="button"
                      onClick={() => setOpen(fn.name)}
                      className="font-mono text-sand underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-sand"
                    >
                      {fn.name}
                    </button>
                  </td>
                  <td className="px-3 py-2">
                    <ReadOnlyHint reason={canChange ? null : VIEWER_REASON}>
                      <Switch
                        checked={enabled}
                        disabled={!canChange || pending}
                        onCheckedChange={(next) => void flip(fn, next)}
                        aria-label={`${fn.name} enabled`}
                      />
                    </ReadOnlyHint>
                    {error && (
                      <p role="alert" className="mt-1 max-w-56 text-xs text-coral">
                        {error}
                      </p>
                    )}
                  </td>
                  <td className="px-3 py-2">
                    <KindCell databaseId={databaseId} branchId={branchId} fn={fn} />
                  </td>
                  <td className="px-3 py-2">
                    {status ? <span className={`text-xs ${TONE[status.tone]}`}>{status.label}</span> : <span className="text-xs text-chalk-dim">–</span>}
                  </td>
                  <td className="max-w-56 px-3 py-2">
                    <ActiveDeployment databaseId={databaseId} branchId={branchId} fn={fn} />
                  </td>
                  <td className="px-3 py-2">{fn.auth ? AUTH_LABEL[fn.auth] : <span className="text-chalk-dim">Not set</span>}</td>
                  <td className="max-w-64 px-3 py-2">
                    {fn.rollout?.state === "pending" ? (
                      <span className="text-xs text-chalk-dim">Rolling over…</span>
                    ) : fn.last_error ? (
                      <span title={fn.last_error} className="line-clamp-2 font-mono text-xs break-all text-coral">
                        {fn.last_error}
                      </span>
                    ) : (
                      <span className="text-chalk-dim">None</span>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {selected && (
        <FunctionDrawer
          fn={selected}
          databaseId={databaseId}
          branchId={branchId}
          apiUrl={apiUrl}
          canChange={canChange}
          update={(body) => update(selected.name, body)}
          onClose={() => setOpen(null)}
        />
      )}
    </div>
  );
}
