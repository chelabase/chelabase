import { useState } from "react";
import { useGetMe } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { BranchRoles } from "./branch-roles";

export interface OwnerPasswordNoticeProps {
  databaseId: string;
  branchId: string;
  onRetry: () => void;
}

/** Shown when a request needs the owner's password and Chelabase doesn't know it (it was set in SQL). */
export function OwnerPasswordNotice({ databaseId, branchId, onRetry }: OwnerPasswordNoticeProps) {
  const [showRoles, setShowRoles] = useState(false);
  return (
    <div className="space-y-2 rounded-lg border p-3 text-sm">
      <p>
        The owner password isn&apos;t known to Chelabase (it was set in SQL), so the console can&apos;t connect as the
        owner. Reset it to continue.
      </p>
      <div className="flex gap-2">
        <Button size="sm" onClick={() => setShowRoles(true)}>
          Reset the owner password
        </Button>
        <Button size="sm" variant="outline" onClick={onRetry}>
          Retry
        </Button>
      </div>
      {showRoles && <RolesOfProject databaseId={databaseId} branchId={branchId} />}
    </div>
  );
}

/** The branch Roles panel needs its project (for the Members links): found from the caller's own projects. */
function RolesOfProject({ databaseId, branchId }: { databaseId: string; branchId: string }) {
  const me = useGetMe();
  const project = me.data?.tenant?.projects.find((p) => p.databases.some((d) => d.database_id === databaseId));
  if (!project) {
    return me.isError ? (
      <p role="alert" className="text-destructive">
        Couldn&apos;t load the roles: {errorText(me.error)}
      </p>
    ) : (
      <p className="text-muted-foreground">Loading roles…</p>
    );
  }
  return <BranchRoles projectId={project.id} databaseId={databaseId} branchId={branchId} />;
}
