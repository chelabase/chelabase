import { useId, useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { ApiError, createTeam, updateTeam, type Team } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { inputClass } from "@/components/table-editor/panel-bits";
import { errorText } from "@/lib/errors";
import { refreshTeams } from "./keys";
import { DESCRIPTION_MAX, NAME_MAX, validateTeamForm, type TeamFormErrors } from "./team-model";

export interface TeamDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Given: edit this team. Absent: create one. */
  team?: Team;
  onSaved: (team: Team) => void;
}

/** Name and description of a team: the create and the edit dialog (tenant admins only; the caller decides). */
export function TeamDialog({ open, onOpenChange, team, onSaved }: TeamDialogProps) {
  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      title={team ? "Edit team" : "Create a team"}
      description="A team gives its members the project access you set on its page."
    >
      <Form key={team?.id ?? "new"} team={team} onClose={() => onOpenChange(false)} onSaved={onSaved} />
    </Modal>
  );
}

function Form({ team, onClose, onSaved }: { team?: Team; onClose: () => void; onSaved: (team: Team) => void }) {
  const queryClient = useQueryClient();
  const nameId = useId();
  const descId = useId();
  const [name, setName] = useState(team?.name ?? "");
  const [description, setDescription] = useState(team?.description ?? "");
  const [errors, setErrors] = useState<TeamFormErrors | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    const invalid = validateTeamForm({ name, description });
    setErrors(invalid);
    setFailure(null);
    if (invalid) return;
    setBusy(true);
    try {
      const body = { name: name.trim(), description };
      const saved = team ? await updateTeam(team.id, body) : await createTeam(body);
      refreshTeams(queryClient);
      onSaved(saved);
      onClose();
    } catch (err) {
      if (err instanceof ApiError && err.code === "name_taken") setErrors({ name: "A team with that name already exists." });
      else setFailure(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form noValidate onSubmit={(e) => void submit(e)} className="space-y-4">
      <div className="space-y-1">
        <label htmlFor={nameId} className="text-sm font-medium">
          Name
        </label>
        <input
          id={nameId}
          autoFocus
          value={name}
          maxLength={NAME_MAX * 2}
          aria-invalid={errors?.name ? true : undefined}
          aria-describedby={errors?.name ? `${nameId}-err` : undefined}
          onChange={(e) => setName(e.target.value)}
          className={inputClass}
        />
        {errors?.name && (
          <p id={`${nameId}-err`} role="alert" className="text-xs text-coral">
            {errors.name}
          </p>
        )}
      </div>
      <div className="space-y-1">
        <label htmlFor={descId} className="text-sm font-medium">
          Description <span className="font-normal text-chalk-dim">(optional)</span>
        </label>
        <textarea
          id={descId}
          rows={3}
          value={description}
          aria-invalid={errors?.description ? true : undefined}
          onChange={(e) => setDescription(e.target.value)}
          className="w-full border border-input bg-ink px-2 py-1.5 text-sm text-chalk outline-none focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand aria-[invalid=true]:border-coral"
        />
        {errors?.description && (
          <p role="alert" className="text-xs text-coral">
            {errors.description}
          </p>
        )}
        <p className="text-xs text-chalk-dim tabular-nums">{`${Array.from(description).length} / ${DESCRIPTION_MAX}`}</p>
      </div>
      {failure && (
        <p role="alert" className="text-sm text-coral">
          {failure}
        </p>
      )}
      <div className="flex justify-end gap-2">
        <Button type="button" variant="ghost" disabled={busy} onClick={onClose}>
          Cancel
        </Button>
        <Button type="submit" disabled={busy}>
          {busy ? "Saving…" : team ? "Save" : "Create team"}
        </Button>
      </div>
    </form>
  );
}
