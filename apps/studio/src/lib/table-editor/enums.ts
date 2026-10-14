import type { Operation, TypeEntry } from "@chelabase/management-client";
import type { Mark } from "./changeset";

export interface ProjectedEnumValue {
  value: string;
  mark: Mark | null;
}

export interface ProjectedEnum {
  /** The current name (operations after a rename name this one). */
  name: string;
  /** The name in the catalog; null for an enum created in this changeset. */
  original: string | null;
  values: ProjectedEnumValue[];
  mark: Mark | null;
}

/**
 * The enums of `schema` as they will look once the pending `ops` apply: the catalog's enums, then each
 * operation of this schema in order. An operation that doesn't apply (a missing enum or value) is skipped;
 * the dry run reports it.
 */
export function projectEnums(catalog: readonly TypeEntry[], ops: readonly Operation[], schema: string): ProjectedEnum[] {
  const out: ProjectedEnum[] = catalog
    .filter((t) => t.kind === "enum" && t.schema === schema)
    .map((t) => ({ name: t.name, original: t.name, values: (t.labels ?? []).map((value) => ({ value, mark: null })), mark: null }));
  const live = (name: string) => out.find((e) => e.name === name && e.mark !== "dropped");
  for (const op of ops) {
    if ("create_enum" in op) {
      const c = op.create_enum;
      if (c.schema === schema && !live(c.name)) out.push({ name: c.name, original: null, values: c.values.map((value) => ({ value, mark: "added" })), mark: "added" });
    } else if ("add_enum_value" in op) {
      const a = op.add_enum_value;
      const e = a.schema === schema ? live(a.name) : undefined;
      if (!e || e.values.some((v) => v.value === a.value)) continue;
      const entry: ProjectedEnumValue = { value: a.value, mark: "added" };
      const rel = a.position ? ("before" in a.position ? a.position.before : a.position.after) : null;
      const at = rel === null ? -1 : e.values.findIndex((v) => v.value === rel);
      if (at < 0) e.values.push(entry);
      else e.values.splice("before" in a.position! ? at : at + 1, 0, entry);
      e.mark ??= "changed";
    } else if ("rename_enum_value" in op) {
      const r = op.rename_enum_value;
      const e = r.schema === schema ? live(r.name) : undefined;
      const v = e?.values.find((x) => x.value === r.from);
      if (e && v && !e.values.some((x) => x.value === r.to)) {
        v.value = r.to;
        v.mark ??= "changed";
        e.mark ??= "changed";
      }
    } else if ("rename_enum" in op) {
      const r = op.rename_enum;
      const e = r.schema === schema ? live(r.name) : undefined;
      if (e && !live(r.to)) {
        e.name = r.to;
        e.mark ??= "changed";
      }
    } else if ("drop_enum" in op) {
      const d = op.drop_enum;
      const e = d.schema === schema ? live(d.name) : undefined;
      if (e) {
        if (e.original === null) out.splice(out.indexOf(e), 1);
        else e.mark = "dropped";
      }
    }
  }
  return out.sort((a, b) => a.name.localeCompare(b.name));
}
