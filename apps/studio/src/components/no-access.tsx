import { useEffect, useRef } from "react";
import { Link } from "@tanstack/react-router";
import { useListMembers, type MeResponse as Me } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { memberName } from "@/lib/members";
import { CopyButton } from "./copy-button";

/** How often the Projects section checks `me` while it shows this screen. */
export const NO_ACCESS_POLL_MS = 30_000;

export interface NoAccessProps {
  me: Me;
  /** A `me` refetch is running ("Check now"). */
  checking: boolean;
  onCheck: () => void;
}

/**
 * Stands in for the Projects section when the caller is a member with no project and isn't a
 * tenant admin: nothing is broken, a tenant admin decides, and here is who they are.
 */
export function NoAccess({ me, checking, onCheck }: NoAccessProps) {
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => heading.current?.focus(), []);
  // A person with no member row can't read the member list (every members route refuses them).
  const isMember = me.member !== null;
  const members = useListMembers({ query: { enabled: isMember } });
  const admins = (members.data ?? []).filter((m) => m.tenant_admin && m.status === "active");
  return (
    <section className="max-w-[65ch] space-y-6">
      <div className="space-y-2">
        <h1 ref={heading} tabIndex={-1} className="text-2xl font-semibold outline-none">
          You don't have access to a project yet
        </h1>
        <p className="text-sm text-chalk">
          {isMember ? (
            <>
              {me.email ? <>You're signed in as {me.email}. </> : null}A tenant admin decides who can use which project;
              ask one of them to give you access.
            </>
          ) : (
            <>
              {me.email ? <>You're signed in as {me.email}, but you</> : <>You</>} aren't a member of this tenant. Ask a
              tenant admin to invite you.
            </>
          )}
        </p>
      </div>
      {isMember && <div className="space-y-2">
        <h2 id="no-access-admins" className="text-sm font-semibold">
          Tenant admins
        </h2>
        {members.isPending ? (
          <p className="text-sm text-chalk-dim">Loading tenant admins…</p>
        ) : members.isError ? (
          <div className="flex flex-wrap items-center gap-3">
            <p role="alert" className="text-sm text-coral">
              Couldn't load the tenant admins: {errorText(members.error)}
            </p>
            <Button size="sm" variant="outline" onClick={() => void members.refetch()}>
              Retry
            </Button>
          </div>
        ) : admins.length === 0 ? (
          <p className="text-sm text-chalk-dim">No tenant admin is listed. Ask whoever runs this Chelabase instance.</p>
        ) : (
          <ul aria-labelledby="no-access-admins" className="border-y border-line">
            {admins.map((a) => (
              <li key={a.id} className="flex min-h-9 flex-wrap items-center gap-x-3 border-b border-line py-1.5 text-sm last:border-b-0">
                <span className="text-chalk">{memberName(a)}</span>
                {a.email ? (
                  <>
                    <span className="text-chalk-dim">{a.email}</span>
                    <CopyButton value={a.email} label={`Copy ${a.email}`} />
                  </>
                ) : (
                  <span className="text-chalk-dim">Email unknown</span>
                )}
              </li>
            ))}
          </ul>
        )}
        <p className="text-sm text-chalk-dim">
          Everyone in this tenant is listed on the{" "}
          <Link to="/app/members" className="text-chalk underline underline-offset-4">
            Members
          </Link>{" "}
          page.
        </p>
      </div>}
      <div className="flex flex-wrap items-center gap-3">
        <p className="text-sm text-chalk-dim">
          This page checks again every 30 seconds, and opens your projects as soon as you have one.
        </p>
        <Button variant="outline" size="sm" disabled={checking} onClick={onCheck}>
          {checking ? "Checking…" : "Check now"}
        </Button>
      </div>
    </section>
  );
}
