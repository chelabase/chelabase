import { useEffect, useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  getGetMeQueryKey,
  getGetProjectQueryKey,
  getListProjectsQueryKey,
  useUpdateProject,
  type DatabaseSummary,
  type ProjectInfo,
} from "@cheladb/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { AppUsersCard } from "./app-users-card";
import { DeleteProjectDialog } from "./delete-project-dialog";

export interface ProjectSettingsProps {
  project: ProjectInfo;
  /** undefined until the workspace has loaded. */
  databases: DatabaseSummary[] | undefined;
  databasesError?: unknown;
  /** The delete was accepted. */
  onDeleted: () => void;
}

export function ProjectSettings({ project, databases, databasesError, onDeleted }: ProjectSettingsProps) {
  const deleting = project.status === "deleting";
  return (
    <fieldset disabled={deleting} className="m-0 min-w-0 space-y-8 border-0 p-0">
      <RenameForm project={project} />
      <AppUsersCard project={project} databases={databases} />
      <DangerZone project={project} databases={databases} databasesError={databasesError} onDeleted={onDeleted} />
    </fieldset>
  );
}

function RenameForm({ project }: { project: ProjectInfo }) {
  const queryClient = useQueryClient();
  const [name, setName] = useState(project.name);
  const [saved, setSaved] = useState(false);
  // Follow the server's name when it changes elsewhere.
  useEffect(() => setName(project.name), [project.name]);
  const update = useUpdateProject({
    mutation: {
      onSuccess: async () => {
        await Promise.all([
          queryClient.invalidateQueries({ queryKey: getGetProjectQueryKey(project.id) }),
          queryClient.invalidateQueries({ queryKey: getGetMeQueryKey() }),
          queryClient.invalidateQueries({ queryKey: getListProjectsQueryKey() }),
        ]);
        setSaved(true);
      },
    },
  });
  const trimmed = name.trim();

  function submit(event: FormEvent) {
    event.preventDefault();
    if (trimmed && trimmed !== project.name && !update.isPending) {
      setSaved(false);
      update.mutate({ id: project.id, data: { name: trimmed } });
    }
  }

  return (
    <section className="space-y-3">
      <h2 className="text-lg font-semibold">General</h2>
      <form onSubmit={submit} className="flex flex-wrap items-end gap-3">
        <label className="flex flex-col gap-1 text-sm">
          <span>Project name</span>
          <input
            value={name}
            onChange={(e) => {
              setName(e.target.value);
              setSaved(false);
            }}
            maxLength={64}
            className="h-8 w-64 rounded-lg border border-input bg-background px-2"
          />
        </label>
        <Button type="submit" disabled={!trimmed || trimmed === project.name || update.isPending}>
          Save
        </Button>
        {saved && <p className="text-sm text-muted-foreground">Saved</p>}
        {update.error && (
          <p role="alert" className="text-sm text-destructive">
            {errorText(update.error)}
          </p>
        )}
      </form>
    </section>
  );
}

function DangerZone({ project, databases, databasesError, onDeleted }: ProjectSettingsProps) {
  const [open, setOpen] = useState(false);
  const [started, setStarted] = useState(false);
  return (
    <section className="space-y-3 rounded-lg border border-destructive/40 p-4">
      <h2 className="text-lg font-semibold text-destructive">Danger zone</h2>
      <p className="text-sm text-muted-foreground">
        Deleting the project destroys all its databases, branches and data. This can't be undone.
      </p>
      <Button variant="destructive" onClick={() => setOpen(true)}>
        Delete project
      </Button>
      {started && <p className="text-sm text-muted-foreground">Delete started</p>}
      <DeleteProjectDialog
        project={project}
        databases={databases}
        databasesError={databasesError}
        open={open}
        onOpenChange={setOpen}
        onStarted={() => {
          setStarted(true);
          onDeleted();
        }}
      />
    </section>
  );
}
