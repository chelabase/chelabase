import { useId, useState, type FormEvent } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { createRole, type BranchRole, type CreateRoleRequest } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { RadioGroup } from "@/components/ui/radio-group";
import { Checkbox, inputClass } from "@/components/table-editor/panel-bits";
import { errorText } from "@/lib/errors";
import { actAsReserved } from "@/lib/role-rules";
import { escapeName } from "../hub/escape-name";
import { Tag } from "./bits";
import { dataApiChainNote, joinableRoles } from "./model";

/** The API's name rule (`CreateRoleRequest.name`); reserved names are refused before asking. */
const NAME_RULE = /^[a-z_][a-z0-9_]{0,62}$/;
const NAME_HELP = "Lowercase letters, digits and _, starting with a letter or _. Up to 63 characters.";
/** The create's reserved names: every act-as reserved one, and the data API's two roles too (they exist already). */
const reservedForCreate = (name: string) => name === "anon" || name === "authenticated" || actAsReserved(name);

type Kind = "group" | "login";

/** "2030-01-31 09:30" (or with a T, or a date alone for midnight), read as the user's local time; anything else, or a day that doesn't exist, is null. */
function localDateTime(text: string): Date | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})(?:[ T](\d{2}):(\d{2}))?$/.exec(text.trim());
  if (!m) return null;
  const [y, mo, d, h, mi] = [Number(m[1]), Number(m[2]), Number(m[3]), Number(m[4] ?? 0), Number(m[5] ?? 0)];
  const date = new Date(y, mo - 1, d, h, mi);
  const same = date.getFullYear() === y && date.getMonth() === mo - 1 && date.getDate() === d && date.getHours() === h && date.getMinutes() === mi;
  return same ? date : null;
}

export interface CreateRoleDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  databaseId: string;
  branchId: string;
  /** "Billing app · billing-prod · main": where the role will live. */
  where: string;
  /** The branch's roles, for "Member of". */
  roles: readonly BranchRole[];
  /** Told the created role's name (the caller goes to its page). */
  onCreated: (name: string) => void;
}

/**
 * "Create a role" (spec Part 4): a name, NOLOGIN by default, and under Advanced the rest of
 * `CreateRoleRequest`. Owner-level stays in the branch Roles panel: it isn't offered here, and
 * neither is joining an owner-level role.
 */
export function CreateRoleDialog({ open, onOpenChange, databaseId, branchId, where, roles, onCreated }: CreateRoleDialogProps) {
  const ids = { name: useId(), help: useId(), hint: useId(), limit: useId(), until: useId(), advanced: useId() };
  const [name, setName] = useState("");
  const [kind, setKind] = useState<Kind>("group");
  const [advanced, setAdvanced] = useState(false);
  const [memberOf, setMemberOf] = useState<string[]>([]);
  const [inherit, setInherit] = useState(true);
  const [bypass, setBypass] = useState(false);
  const [limit, setLimit] = useState("");
  const [until, setUntil] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const trimmed = name.trim();
  const limitValue = limit.trim() === "" ? null : Number(limit);
  const nameHint =
    trimmed === "" ? null : !NAME_RULE.test(trimmed) ? NAME_HELP : reservedForCreate(trimmed) ? `${trimmed} is a reserved name.` : roles.some((r) => r.name === trimmed) ? `A role named ${trimmed} already exists on this branch.` : null;
  const limitHint = limitValue !== null && (!Number.isInteger(limitValue) || limitValue < -1) ? "A whole number, or empty for no limit." : null;
  const untilValue = until.trim() === "" ? null : localDateTime(until);
  const untilHint = until.trim() !== "" && untilValue === null ? "Pick a date and time, or leave it empty." : null;
  const ready = trimmed !== "" && nameHint === null && limitHint === null && untilHint === null && !busy;
  const choices = joinableRoles(roles);

  function reset() {
    setName("");
    setKind("group");
    setAdvanced(false);
    setMemberOf([]);
    setInherit(true);
    setBypass(false);
    setLimit("");
    setUntil("");
    setError(null);
  }

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (!ready) return;
    const body: CreateRoleRequest = { name: trimmed, login: kind === "login", member_of: memberOf };
    if (!inherit) body.inherit = false;
    if (bypass) body.bypass_rls = true;
    if (limitValue !== null && limitValue !== -1) body.connection_limit = limitValue;
    if (untilValue !== null) body.valid_until = untilValue.toISOString();
    setBusy(true);
    setError(null);
    try {
      const made = await createRole(databaseId, branchId, body);
      reset();
      onCreated(made.name);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal
      open={open}
      onOpenChange={(next) => {
        if (!next) reset();
        onOpenChange(next);
      }}
      title="Create a role"
      description={`On ${where}. This branch only.`}
      size="medium"
    >
      <form onSubmit={(e) => void submit(e)} className="space-y-4">
        <div className="space-y-1.5">
          <label htmlFor={ids.name} className="text-sm font-medium">
            Name
          </label>
          <input
            id={ids.name}
            value={name}
            onChange={(e) => setName(e.target.value)}
            maxLength={63}
            autoComplete="off"
            spellCheck={false}
            aria-invalid={nameHint ? true : undefined}
            aria-describedby={nameHint ? ids.hint : ids.help}
            className={`${inputClass} font-mono`}
          />
          {nameHint ? (
            <p id={ids.hint} className="text-xs text-coral">
              {nameHint}
            </p>
          ) : (
            <p id={ids.help} className="text-xs text-chalk-dim">
              {NAME_HELP}
            </p>
          )}
        </div>

        <RadioGroup<Kind>
          label="Kind"
          value={kind}
          onValueChange={setKind}
          options={[
            { value: "group", label: "Group role (NOLOGIN)", hint: "Members act as it. Nobody signs in as it." },
            { value: "login", label: "Login role", hint: "An app or service signs in as it with a password." },
          ]}
        />

        <div className="border-t border-line pt-3">
          <button
            type="button"
            aria-expanded={advanced}
            aria-controls={ids.advanced}
            onClick={() => setAdvanced((a) => !a)}
            className="inline-flex items-center gap-1.5 text-sm font-medium text-chalk outline-none focus-visible:outline-2 focus-visible:outline-sand"
          >
            {advanced ? <ChevronDown aria-hidden="true" size={16} strokeWidth={1.5} /> : <ChevronRight aria-hidden="true" size={16} strokeWidth={1.5} />}
            Advanced
          </button>
          {advanced && (
            <div id={ids.advanced} className="mt-3 space-y-3 pl-6">
              <fieldset className="space-y-1.5">
                <legend className="text-[13px] font-medium">Member of</legend>
                {choices.length === 0 ? (
                  <p className="text-[13px] text-chalk-dim">No other roles on this branch yet.</p>
                ) : (
                  <ul className="max-h-48 divide-y divide-line overflow-y-auto border-y border-line">
                    {choices.map((r) => {
                      const note = dataApiChainNote(roles, r.name);
                      return (
                        <li key={r.name}>
                          <label className="flex min-h-8 items-center gap-2 py-1 text-sm">
                            <Checkbox
                              checked={memberOf.includes(r.name)}
                              onChange={() => setMemberOf((m) => (m.includes(r.name) ? m.filter((v) => v !== r.name) : [...m, r.name]))}
                            />
                            <span className="font-mono text-[13px] break-all text-chalk">{escapeName(r.name)}</span>
                            {note && <span className="ml-auto text-xs text-chalk-dim">{note}</span>}
                          </label>
                        </li>
                      );
                    })}
                  </ul>
                )}
              </fieldset>
              <label className="flex h-8 items-center gap-2 text-sm">
                <Checkbox checked={inherit} onChange={(e) => setInherit(e.target.checked)} />
                <span>Inherit privileges of the roles it's in</span>
              </label>
              <div className="flex items-center gap-2">
                <label className="flex h-8 items-center gap-2 text-sm">
                  <Checkbox checked={bypass} onChange={(e) => setBypass(e.target.checked)} />
                  <span>Bypass row-level security</span>
                </label>
                <Tag hazard>owner-level</Tag>
              </div>
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <div className="space-y-1.5">
                  <label htmlFor={ids.limit} className="text-[13px] font-medium">
                    Connection limit
                  </label>
                  <input
                    id={ids.limit}
                    type="number"
                    min={-1}
                    step={1}
                    inputMode="numeric"
                    placeholder="No limit"
                    value={limit}
                    onChange={(e) => setLimit(e.target.value)}
                    aria-invalid={limitHint ? true : undefined}
                    className={inputClass}
                  />
                  {limitHint && <p className="text-xs text-coral">{limitHint}</p>}
                </div>
                <div className="space-y-1.5">
                  <label htmlFor={ids.until} className="text-[13px] font-medium">
                    Valid until
                  </label>
                  <input
                    id={ids.until}
                    type="text"
                    inputMode="numeric"
                    autoComplete="off"
                    placeholder="2030-01-31 09:30"
                    value={until}
                    onChange={(e) => setUntil(e.target.value)}
                    aria-invalid={untilHint ? true : undefined}
                    className={inputClass}
                  />
                  <p className="text-xs text-chalk-dim">{untilHint ?? "Empty: never expires. Your local time."}</p>
                </div>
              </div>
            </div>
          )}
        </div>

        {error && (
          <p role="alert" className="text-sm text-coral">
            {error}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <Button
            type="button"
            variant="ghost"
            onClick={() => {
              reset();
              onOpenChange(false);
            }}
          >
            Cancel
          </Button>
          <Button type="submit" disabled={!ready}>
            Create role
          </Button>
        </div>
      </form>
    </Modal>
  );
}
