import { Button } from "@/components/ui/button";
import { useTableEditor } from "@/lib/table-editor/store";
import type { TableTarget } from "./target";
import { useTableStructure } from "./use-structure";

export const RLS_OFF = "RLS is off: any role with grants reads and writes every row";
export const RLS_NO_POLICIES = "RLS is on with no policies: only the owner and roles that bypass RLS see rows.";

/**
 * Over the Rows and Structure views of a table: a coral banner while RLS is off (with "Turn RLS on",
 * staging `set_rls { enabled: true, forced: false }`, S7), or a note when RLS is on without a policy.
 */
export function RlsBanner({ target, reason, enabled }: { target: TableTarget; reason: string | null; enabled: boolean }) {
  const { base, projected, ref, stage, applying, reason: dropped } = useTableStructure(target, { enabled });
  const openPanel = useTableEditor((s) => s.openPanel);
  if (!base || !projected) return null;
  if (!base.rls_enabled) {
    const staged = projected.rls_enabled;
    return (
      <div role="alert" className="flex items-center gap-4 border-b border-coral/60 bg-coral/[0.08] px-4 py-2 text-sm">
        <p className="flex-1 text-chalk">
          {RLS_OFF}
          {staged && <span className="text-chalk-dim"> · turning it on is in your pending changes</span>}
        </p>
        {!staged && dropped === null && (
          <Button
            size="sm"
            variant="outline"
            disabled={reason !== null || applying}
            title={reason ?? undefined}
            onClick={() => {
              stage([{ set_rls: { table: ref, enabled: true, forced: false } }]);
              openPanel("changes");
            }}
          >
            Turn RLS on
          </Button>
        )}
      </div>
    );
  }
  if (base.policies.length === 0) {
    return <p className="border-b border-line px-4 py-2 text-[13px] text-chalk-dim">{RLS_NO_POLICIES}</p>;
  }
  return null;
}
