import { useEffect, useState } from "react";
import { useListAppUsers } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { appUserErrorText } from "@/lib/app-user-errors";
import { CreateUserDialog } from "./create-user-dialog";
import { UserDrawer } from "./user-drawer";

const DEBOUNCE_MS = 300;

export function UsersTab({ projectId }: { projectId: string }) {
  const [input, setInput] = useState("");
  const [search, setSearch] = useState("");
  // The page tokens walked so far: the last one is the current page's.
  const [tokens, setTokens] = useState<string[]>([]);
  const [creating, setCreating] = useState(false);
  const [openId, setOpenId] = useState<string | null>(null);

  useEffect(() => {
    const t = setTimeout(() => {
      const next = input.trim();
      if (next !== search) {
        setSearch(next);
        setTokens([]);
      }
    }, DEBOUNCE_MS);
    return () => clearTimeout(t);
  }, [input, search]);

  const params = { ...(search ? { search } : {}), ...(tokens.length ? { page_token: tokens[tokens.length - 1] } : {}) };
  const list = useListAppUsers(projectId, params, { query: { placeholderData: (prev) => prev } });
  const next = list.data?.next_page_token ?? null;

  return (
    <section className="space-y-4">
      <div className="flex flex-wrap items-center gap-3">
        <input
          type="search"
          aria-label="Search users"
          placeholder="Search by email"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          className="h-8 w-64 max-w-full rounded-lg border border-input bg-background px-2 text-sm"
        />
        <Button onClick={() => setCreating(true)}>Create user</Button>
      </div>
      {list.error && (
        <p role="alert" className="text-sm text-destructive">
          {appUserErrorText(list.error)}
        </p>
      )}
      {list.data === undefined ? (
        !list.error && <p className="text-sm text-muted-foreground">Loading users…</p>
      ) : list.data.users.length === 0 ? (
        <p className="text-sm text-muted-foreground">{search ? "No users match." : "No users yet."}</p>
      ) : (
        <table className="w-full text-left text-sm">
          <thead className="border-b text-muted-foreground">
            <tr>
              <th className="py-2 font-medium">Email</th>
              <th className="py-2 font-medium">Verified</th>
              <th className="py-2 font-medium">State</th>
              <th className="py-2 font-medium">Created</th>
            </tr>
          </thead>
          <tbody>
            {list.data.users.map((u) => (
              <tr key={u.id} className="border-b last:border-0">
                <td className="py-2">
                  <button type="button" className="underline-offset-4 hover:underline" onClick={() => setOpenId(u.id)}>
                    {u.email}
                  </button>
                </td>
                <td className="py-2">{u.email_verified ? "Yes" : "No"}</td>
                <td className="py-2">{u.state}</td>
                <td className="py-2">{new Date(u.created_at).toLocaleString()}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <div className="flex gap-2">
        <Button variant="outline" disabled={tokens.length === 0} onClick={() => setTokens((t) => t.slice(0, -1))}>
          Previous
        </Button>
        <Button variant="outline" disabled={next === null} onClick={() => next && setTokens((t) => [...t, next])}>
          Next
        </Button>
      </div>
      <CreateUserDialog projectId={projectId} open={creating} onOpenChange={setCreating} />
      {openId && <UserDrawer projectId={projectId} userId={openId} onClose={() => setOpenId(null)} />}
    </section>
  );
}
