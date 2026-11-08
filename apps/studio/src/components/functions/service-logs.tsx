import { useQuery } from "@tanstack/react-query";
import { getGetServiceLogsQueryKey, getServiceLogs } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { functionErrorText } from "@/lib/functions";

/**
 * A container service's last 200 log lines on the branch. Reading them needs Developer (the drawer
 * shows a Viewer a hint instead); any refusal is shown rather than an empty panel.
 */
export function ServiceLogs({ databaseId, branchId, name }: { databaseId: string; branchId: string; name: string }) {
  const logs = useQuery({
    queryKey: getGetServiceLogsQueryKey(databaseId, branchId, name),
    queryFn: async () => (await getServiceLogs(databaseId, branchId, name)) ?? "",
    retry: false,
    staleTime: 0,
  });
  return (
    <div className="space-y-2 text-sm">
      <div className="flex items-center justify-between gap-2">
        <p className="text-xs text-chalk-dim">The last 200 lines from the active deployment&apos;s container.</p>
        <Button size="xs" variant="outline" disabled={logs.isFetching} onClick={() => void logs.refetch()} aria-label="Refresh logs">
          {logs.isFetching ? "Reading…" : "Refresh"}
        </Button>
      </div>
      {logs.error ? (
        <p role="alert" className="text-coral">
          {functionErrorText(logs.error)}
        </p>
      ) : logs.data === undefined ? (
        <p className="text-chalk-dim">Loading…</p>
      ) : logs.data.trim() === "" ? (
        <p className="text-chalk-dim">No log lines yet.</p>
      ) : (
        <pre tabIndex={0} aria-label={`${name} logs`} className="max-h-80 overflow-auto border border-line bg-ink p-2 font-mono text-xs leading-5 whitespace-pre-wrap break-all">
          {logs.data}
        </pre>
      )}
    </div>
  );
}
