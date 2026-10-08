import type { ReactNode } from "react";
import { Link } from "@tanstack/react-router";

export const MERGE_ROUTE = "/app/projects/$projectId/databases/$databaseId/merges/$mergeId" as const;

export interface MergeLinkProps {
  projectId: string;
  databaseId: string;
  mergeId: string;
  children: ReactNode;
  className?: string;
}

/** A link to a merge's page. */
export function MergeLink({ projectId, databaseId, mergeId, children, className }: MergeLinkProps) {
  return (
    <Link
      to={MERGE_ROUTE}
      params={{ projectId, databaseId, mergeId }}
      className={className ?? "text-foreground underline underline-offset-4"}
    >
      {children}
    </Link>
  );
}
