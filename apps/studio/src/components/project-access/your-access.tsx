import type { MeResponse as Me, PlatformRole, Project } from "@chelabase/management-client";
import { ROLE_HINT } from "@/components/members/platform-role-field";
import { projectCaps } from "@/lib/capabilities";
import { ROLE_LABEL } from "@/lib/members";
import { chainsOf, useRoleSources } from "./use-role-sources";

const article = (label: string) => (/^[aeiou]/i.test(label) ? "an" : "a");

/** The caller's own access on this project, read from `/v1/me` (no Admin-only route is called). */
export function YourAccess({ project, me, compact = false }: { project: Project; me: Me; compact?: boolean }) {
  const caps = projectCaps(me, project.id);

  if (caps.role === null) {
    return (
      <section aria-labelledby="your-access" className="space-y-2">
        <h3 id="your-access" className="text-base font-semibold">
          Your access
        </h3>
        <p className="text-sm text-chalk-dim">You don't have access to {project.name}.</p>
      </section>
    );
  }
  const label = ROLE_LABEL[caps.role];
  if (compact) {
    return (
      <section aria-labelledby="your-access" className="space-y-1">
        <h3 id="your-access" className="text-base font-semibold">
          Your access
        </h3>
        <p className="text-sm text-chalk">
          {caps.tenantAdmin
            ? "You're a tenant admin: Admin on every project."
            : `You're ${article(label)} ${label} on ${project.name}${caps.role === "admin" ? ", including the database owner" : ""}.`}
        </p>
      </section>
    );
  }
  return <RolesAccess project={project} role={caps.role} roles={caps.roles} label={label} />;
}

/** The full view (below Admin): roles with their chains, which need the databases' role lists. */
function RolesAccess({ project, role, roles, label }: { project: Project; role: PlatformRole; roles: "all" | string[]; label: string }) {
  const { sources } = useRoleSources(project.databases);
  const chains = chainsOf(sources);
  return (
    <section aria-labelledby="your-access" className="space-y-3">
      <h3 id="your-access" className="text-base font-semibold">
        Your access
      </h3>
      <p className="text-sm text-chalk">
        You're {article(label)} {label} on {project.name}.
      </p>
      {roles === "all" ? (
        // R17: an Admin holds every role of the project.
        <p className="text-sm text-chalk">You may act as every role of the project, the database owner included.</p>
      ) : roles.length > 0 ? (
        <div className="flex flex-wrap items-center gap-2 text-sm">
          <span className="text-chalk">You may act as</span>
          {roles.map((r) => {
            const chain = chains.get(r) ?? [];
            return (
              <span key={r} className="inline-flex items-center gap-1 border border-line-strong px-1.5 leading-6">
                <span className="font-mono text-[13px]">{r}</span>
                {chain.length > 0 && <span className="text-chalk-dim">+ {chain.join(", ")}</span>}
              </span>
            );
          })}
        </div>
      ) : (
        <p className="text-sm text-chalk">You can't act as any Postgres role here.</p>
      )}
      <p className="max-w-[65ch] text-sm text-chalk-dim">{ROLE_HINT[role]}</p>
      <p className="text-sm text-chalk-dim">Some databases may narrow this further.</p>
      <p className="max-w-[65ch] text-sm text-chalk-dim">
        Project Admins and tenant admins decide who has access. Ask one of them to change yours.
      </p>
    </section>
  );
}
