import { useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { getGetMeQueryKey, getListProjectsQueryKey, useCreateProject, type ProjectInfo } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { errorText } from "@/lib/errors";

export function NewProjectDialog({ onCreated }: { onCreated: (project: ProjectInfo) => void }) {
  const queryClient = useQueryClient();
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const create = useCreateProject({
    mutation: {
      onSuccess: async (project) => {
        await Promise.all([
          queryClient.invalidateQueries({ queryKey: getGetMeQueryKey() }),
          queryClient.invalidateQueries({ queryKey: getListProjectsQueryKey() }),
        ]);
        setOpen(false);
        setName("");
        onCreated(project);
      },
    },
  });
  const trimmed = name.trim();

  function submit(event: FormEvent) {
    event.preventDefault();
    if (trimmed && !create.isPending) create.mutate({ data: { name: trimmed } });
  }

  return (
    <>
      <Button onClick={() => { create.reset(); setOpen(true); }}>New project</Button>
      <Modal open={open} onOpenChange={setOpen} title="New project">
        <form onSubmit={submit} className="space-y-4">
          <label className="flex flex-col gap-1 text-sm">
            <span>Project name</span>
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              disabled={create.isPending}
              maxLength={64}
              className="h-8 rounded-lg border border-input bg-background px-2"
              placeholder="Storefront"
            />
          </label>
          {create.error && (
            <p role="alert" className="text-sm text-destructive">
              {errorText(create.error)}
            </p>
          )}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>
              Cancel
            </Button>
            <Button type="submit" disabled={!trimmed || create.isPending}>
              Create project
            </Button>
          </div>
        </form>
      </Modal>
    </>
  );
}
