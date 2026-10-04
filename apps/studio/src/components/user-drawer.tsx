import { useEffect, useState, type FormEvent, type ReactNode } from "react";
import { Dialog as DialogPrimitive } from "@base-ui/react/dialog";
import { Check } from "lucide-react";
import { useQueryClient, type QueryClient } from "@tanstack/react-query";
import {
  getGetAppUserQueryKey,
  getListAppUserSessionsQueryKey,
  useDeactivateAppUser,
  useDeleteAppUser,
  useGetAppUser,
  useListAppUserSessions,
  useReactivateAppUser,
  useRevokeAppUserSessions,
  useUpdateAppUser,
  type AppUserPage,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { appUserErrorText } from "@/lib/app-user-errors";

/** Refreshes the user list and every user's detail and sessions. */
export function invalidateUsers(queryClient: QueryClient, projectId: string) {
  const prefix = `/v1/projects/${projectId}/users`;
  return queryClient.invalidateQueries({
    predicate: (q) => typeof q.queryKey[0] === "string" && q.queryKey[0].startsWith(prefix),
  });
}

const input = "h-8 rounded-lg border border-input bg-background px-2 text-sm";

function Err({ error }: { error: unknown }) {
  return error ? (
    <p role="alert" className="text-sm text-destructive">
      {appUserErrorText(error)}
    </p>
  ) : null;
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="space-y-2 border-t pt-4">
      <h3 className="text-sm font-semibold">{title}</h3>
      {children}
    </section>
  );
}

export interface UserDrawerProps {
  projectId: string;
  userId: string;
  onClose: () => void;
}

/** A side panel with one user's details and actions. */
export function UserDrawer({ projectId, userId, onClose }: UserDrawerProps) {
  const queryClient = useQueryClient();
  const refresh = () => invalidateUsers(queryClient, projectId);
  const user = useGetAppUser(projectId, userId);
  const sessions = useListAppUserSessions(projectId, userId);

  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [passwordChanged, setPasswordChanged] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [typed, setTyped] = useState("");
  useEffect(() => {
    if (user.data) setEmail(user.data.email);
  }, [user.data?.email]);

  const update = useUpdateAppUser({ mutation: { onSuccess: refresh } });
  const setPw = useUpdateAppUser({
    mutation: {
      onSuccess: async () => {
        setPasswordChanged(true);
        await refresh();
      },
      onSettled: () => setPassword(""),
    },
  });
  const deactivate = useDeactivateAppUser({ mutation: { onSuccess: refresh } });
  const reactivate = useReactivateAppUser({ mutation: { onSuccess: refresh } });
  const revoke = useRevokeAppUserSessions({ mutation: { onSuccess: refresh } });
  const remove = useDeleteAppUser({
    mutation: {
      onSuccess: async () => {
        onClose();
        queryClient.removeQueries({ queryKey: getGetAppUserQueryKey(projectId, userId) });
        queryClient.removeQueries({ queryKey: getListAppUserSessionsQueryKey(projectId, userId) });
        // Zitadel's list lags behind a delete: drop the user from every cached page now and only mark the lists stale
        // (an immediate refetch could bring the deleted user back).
        queryClient.setQueriesData<AppUserPage>({ queryKey: ["/v1/projects/" + projectId + "/users"] }, (old) =>
          old && Array.isArray(old.users) ? { ...old, users: old.users.filter((x) => x.id !== userId) } : old,
        );
        await queryClient.invalidateQueries({ queryKey: ["/v1/projects/" + projectId + "/users"], refetchType: "none" });
      },
    },
  });

  const u = user.data;
  const ids = { id: projectId, userId };

  function saveEmail(event: FormEvent) {
    event.preventDefault();
    if (u && email.trim() && email.trim() !== u.email && !update.isPending) update.mutate({ ...ids, data: { email: email.trim() } });
  }
  function savePassword(event: FormEvent) {
    event.preventDefault();
    if (password && !setPw.isPending) {
      setPasswordChanged(false);
      setPw.mutate({ ...ids, data: { password } });
    }
  }

  return (
    <DialogPrimitive.Root open onOpenChange={(open) => !open && onClose()}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Backdrop className="fixed inset-0 bg-black/40" />
        <DialogPrimitive.Popup className="fixed top-0 right-0 h-full w-[28rem] max-w-full space-y-4 overflow-y-auto border-l bg-background p-6 text-foreground shadow-lg">
          <DialogPrimitive.Title className="text-lg font-semibold break-all">{u?.email ?? "User"}</DialogPrimitive.Title>
          {!u ? (
            <p className="text-sm text-muted-foreground">{user.error ? appUserErrorText(user.error) : "Loading…"}</p>
          ) : (
            <>
              <p className="text-sm text-muted-foreground">
                {u.state} · created {new Date(u.created_at).toLocaleString()}
              </p>
              <Section title="Email">
                <form onSubmit={saveEmail} className="flex flex-wrap items-end gap-2">
                  <label className="flex flex-col gap-1 text-sm">
                    <span>Email</span>
                    <input value={email} onChange={(e) => setEmail(e.target.value)} autoComplete="off" className={`${input} w-64`} />
                  </label>
                  <Button type="submit" disabled={!email.trim() || email.trim() === u.email || update.isPending}>
                    Save email
                  </Button>
                </form>
                {/* Zitadel can't take a verification back without a new address: once verified, it stays. */}
                {u.email_verified ? (
                  <p className="flex items-center gap-1.5 text-sm">
                    <Check className="size-4 text-green-600" aria-hidden="true" />
                    <span>Verified</span>
                  </p>
                ) : (
                  <div className="flex items-center gap-2 text-sm">
                    <span className="text-muted-foreground">Not verified</span>
                    <Button
                      type="button"
                      variant="outline"
                      size="sm"
                      disabled={update.isPending}
                      onClick={() => update.mutate({ ...ids, data: { email_verified: true } })}
                    >
                      Mark verified
                    </Button>
                  </div>
                )}
                <Err error={update.error} />
              </Section>
              <Section title="Password">
                <form onSubmit={savePassword} className="flex flex-wrap items-end gap-2">
                  <label className="flex flex-col gap-1 text-sm">
                    <span>New password</span>
                    <input
                      type="password"
                      value={password}
                      onChange={(e) => setPassword(e.target.value)}
                      autoComplete="new-password"
                      className={`${input} w-64`}
                    />
                  </label>
                  <Button type="submit" disabled={!password || setPw.isPending}>
                    Set password
                  </Button>
                </form>
                {passwordChanged && <p className="text-sm text-muted-foreground">Password changed; the user's sessions ended</p>}
                <Err error={setPw.error} />
              </Section>
              <Section title="Access">
                {u.state === "inactive" ? (
                  <Button variant="outline" disabled={reactivate.isPending} onClick={() => reactivate.mutate(ids)}>
                    Reactivate
                  </Button>
                ) : (
                  <Button variant="outline" disabled={deactivate.isPending} onClick={() => deactivate.mutate(ids)}>
                    Deactivate
                  </Button>
                )}
                <Err error={deactivate.error ?? reactivate.error} />
              </Section>
              <Section title="Sessions">
                {sessions.data === undefined ? (
                  <p className="text-sm text-muted-foreground">{sessions.error ? appUserErrorText(sessions.error) : "Loading…"}</p>
                ) : sessions.data.length === 0 ? (
                  <p className="text-sm text-muted-foreground">No active sessions</p>
                ) : (
                  <ul className="space-y-1 text-sm">
                    {sessions.data.map((s) => (
                      <li key={s.id}>
                        Started {new Date(s.created_at).toLocaleString()} · Last used {new Date(s.last_used_at).toLocaleString()} · expires{" "}
                        {new Date(s.expires_at).toLocaleString()}
                      </li>
                    ))}
                  </ul>
                )}
                <Button variant="outline" disabled={revoke.isPending} onClick={() => { revoke.reset(); revoke.mutate(ids); }}>
                  Sign out everywhere
                </Button>
                {revoke.isSuccess && <p className="text-sm text-muted-foreground">Signed out of all sessions</p>}
                <Err error={revoke.error} />
              </Section>
              <Section title="Delete">
                {confirming ? (
                  <div className="space-y-2">
                    <label className="flex flex-col gap-1 text-sm">
                      <span>
                        Type <code>{u.email}</code> to confirm
                      </span>
                      <input value={typed} onChange={(e) => setTyped(e.target.value)} autoComplete="off" className={input} />
                    </label>
                    <div className="flex gap-2">
                      <Button variant="outline" onClick={() => { setConfirming(false); setTyped(""); }}>
                        Cancel
                      </Button>
                      <Button variant="destructive" disabled={typed !== u.email || remove.isPending} onClick={() => remove.mutate(ids)}>
                        Delete user permanently
                      </Button>
                    </div>
                  </div>
                ) : (
                  <Button variant="destructive" onClick={() => setConfirming(true)}>
                    Delete user
                  </Button>
                )}
                <Err error={remove.error} />
              </Section>
            </>
          )}
          <div className="flex justify-end">
            <Button variant="outline" onClick={onClose}>
              Close
            </Button>
          </div>
        </DialogPrimitive.Popup>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}
