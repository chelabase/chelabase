import { useEffect, useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useCreateAppUser } from "@cheladb/management-client";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { appUserErrorText } from "@/lib/app-user-errors";
import { invalidateUsers } from "./user-drawer";

const input = "h-8 rounded-lg border border-input bg-background px-2";

export function CreateUserDialog({ projectId, open, onOpenChange }: { projectId: string; open: boolean; onOpenChange: (open: boolean) => void }) {
  const queryClient = useQueryClient();
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [verified, setVerified] = useState(true);
  const create = useCreateAppUser({
    mutation: {
      onSuccess: async () => {
        await invalidateUsers(queryClient, projectId);
        onOpenChange(false);
      },
    },
  });
  useEffect(() => {
    if (!open) {
      setEmail("");
      setPassword("");
      setVerified(true);
      create.reset();
    }
  }, [open]);

  function submit(event: FormEvent) {
    event.preventDefault();
    if (email.trim() && password && !create.isPending) {
      create.mutate({ id: projectId, data: { email: email.trim(), password, email_verified: verified } });
    }
  }

  return (
    <Modal open={open} onOpenChange={onOpenChange} title="Create user">
      <form onSubmit={submit} className="space-y-4">
        <label className="flex flex-col gap-1 text-sm">
          <span>Email</span>
          <input type="email" value={email} onChange={(e) => setEmail(e.target.value)} autoComplete="off" className={input} />
        </label>
        <label className="flex flex-col gap-1 text-sm">
          <span>Password</span>
          <input type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="new-password" className={input} />
        </label>
        <label className="flex items-center gap-2 text-sm">
          <input type="checkbox" checked={verified} onChange={(e) => setVerified(e.target.checked)} />
          <span>Email is verified (no email is sent)</span>
        </label>
        {create.error && (
          <p role="alert" className="text-sm text-destructive">
            {appUserErrorText(create.error)}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" disabled={!email.trim() || !password || create.isPending}>
            Create
          </Button>
        </div>
      </form>
    </Modal>
  );
}
