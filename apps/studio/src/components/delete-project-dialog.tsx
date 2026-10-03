import { useEffect, useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  ApiError,
  getGetMeQueryKey,
  getGetProjectQueryKey,
  getListProjectsQueryKey,
  useDeleteProject,
  type DatabaseSummary,
  type ProjectInfo,
} from "@cheladb/management-client";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { errorText } from "@/lib/errors";

/** Plain texts for the 409s a project delete can answer with. */
const REFUSALS: Record<string, string> = {
  project_deleting: "This project is already being deleted.",
  database_not_ready: "A database of this project is still being created or changed. Try again when it is ready.",
  branch_protected: "A branch of this project is protected. Unprotect it first, then try again.",
  conflict: "App users are being enabled or disabled. Try again in a moment.",
};

export function deleteRefusalText(err: unknown): string {
  return err instanceof ApiError && err.status === 409 && REFUSALS[err.code] ? REFUSALS[err.code] : errorText(err);
}

export interface DeleteProjectDialogProps {
  project: Pick<ProjectInfo, "id" | "slug" | "name">;
  /** undefined until the workspace has loaded. */
  databases: DatabaseSummary[] | undefined;
  /** Why the databases couldn't be loaded, if so. */
  databasesError?: unknown;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The delete was accepted (the project is now `deleting`). */
  onStarted: () => void;
}

export function DeleteProjectDialog({ project, databases, databasesError, open, onOpenChange, onStarted }: DeleteProjectDialogProps) {
  const queryClient = useQueryClient();
  const [typed, setTyped] = useState("");
  const remove = useDeleteProject({
    mutation: {
      onSuccess: async () => {
        await Promise.all([
          queryClient.invalidateQueries({ queryKey: getGetProjectQueryKey(project.id) }),
          queryClient.invalidateQueries({ queryKey: getGetMeQueryKey() }),
          queryClient.invalidateQueries({ queryKey: getListProjectsQueryKey() }),
        ]);
        onOpenChange(false);
        onStarted();
      },
    },
  });

  // However the dialog closes (Cancel, Escape, success), the next opening starts clean.
  useEffect(() => {
    if (!open) {
      setTyped("");
      remove.reset();
    }
  }, [open]);

  function submit(event: FormEvent) {
    event.preventDefault();
    if (databases !== undefined && typed === project.slug && !remove.isPending) remove.mutate({ id: project.id, data: { confirm: project.slug } });
  }

  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      title={`Delete ${project.name}`}
    >
      <form onSubmit={submit} className="space-y-4">
        {databases === undefined ? (
          <p className="text-sm">
            {databasesError ? `Couldn't load the databases: ${errorText(databasesError)}` : "Loading databases…"}
          </p>
        ) : databases.length === 0 ? (
          <p className="text-sm">This project has no databases.</p>
        ) : (
          <div className="space-y-1 text-sm">
            <p>These databases will be destroyed, with all their branches and data:</p>
            <ul className="list-disc pl-5">
              {databases.map((db) => (
                <li key={db.database_id}>{db.name}</li>
              ))}
            </ul>
          </div>
        )}
        <label className="flex flex-col gap-1 text-sm">
          <span>
            Type <code>{project.slug}</code> to confirm
          </span>
          <input
            value={typed}
            onChange={(e) => setTyped(e.target.value)}
            disabled={remove.isPending}
            autoComplete="off"
            className="h-8 rounded-lg border border-input bg-background px-2"
          />
        </label>
        {remove.error && (
          <p role="alert" className="text-sm text-destructive">
            {deleteRefusalText(remove.error)}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" variant="destructive" disabled={databases === undefined || typed !== project.slug || remove.isPending}>
            Delete project and its databases
          </Button>
        </div>
      </form>
    </Modal>
  );
}
