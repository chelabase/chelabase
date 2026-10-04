import { useState } from "react";
import { Button } from "@/components/ui/button";
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
      {showRoles && <BranchRoles databaseId={databaseId} branchId={branchId} />}
    </div>
  );
}
