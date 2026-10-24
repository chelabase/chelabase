import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ApiError,
  applySchemaChanges,
  getGetPrivilegesQueryKey,
  getPrivileges,
  type ObjectPrivileges,
  type PrivilegesPage,
  type PrivilegeTarget,
  type SchemaChangeRequest,
  type SchemaChangeResponse,
} from "@chelabase/management-client";
import { errorText } from "@/lib/errors";
import { editKey, expectAclOf, indexPrivileges, operationOf, stillChanges, type Edit, type PrivilegeIndex } from "./model";

/** One mismatch of a 409 `privileges_not_applied` (MH-24); `privilege` is upper case. */
export interface Mismatch {
  object: PrivilegeTarget;
  privilege: string;
  grantee: string;
  still_granted_by: string | null;
  column: string | null;
  wanted: boolean;
}

/** Why a dry run or an apply didn't go through. */
export type Failure =
  | { kind: "stale"; objects: PrivilegeTarget[] }
  | { kind: "not_applied"; mismatches: Mismatch[]; message: string }
  | { kind: "exposes"; chain: string[]; opIndex: number | null; message: string }
  | { kind: "error"; message: string };

export interface Review {
  /** What the dry run sent; Apply sends the same, for real. */
  body: SchemaChangeRequest;
  /** The edits it stands for, in order (the n-th operation is the n-th edit). */
  edits: Edit[];
  running: boolean;
  applying: boolean;
  /** The dry run's answer. */
  response: SchemaChangeResponse | null;
  failure: Failure | null;
  /**
   * After a refusal that names one change (`exposes_data_api`): the dry run of the others, so the
   * review keeps its SQL and says which changes passed. Never applied: the refusal stays.
   */
  partial?: { response: SchemaChangeResponse; /** Indices into `edits`. */ passed: number[] } | null;
}

export interface SchemaRead {
  objects: ObjectPrivileges[];
  /** More objects after these (`Load more`). */
  more: boolean;
  loading: boolean;
  error: unknown;
}

const isTarget = (v: unknown): v is PrivilegeTarget => typeof v === "object" && v !== null && !Array.isArray(v) && Object.keys(v).length === 1;
const str = (v: unknown): string | null => (typeof v === "string" ? v : null);

function mismatchesOf(v: unknown): Mismatch[] {
  if (!Array.isArray(v)) return [];
  return v.flatMap((m): Mismatch[] => {
    if (typeof m !== "object" || m === null) return [];
    const r = m as Record<string, unknown>;
    if (!isTarget(r.object) || typeof r.privilege !== "string") return [];
    return [{ object: r.object, privilege: r.privilege, grantee: str(r.grantee) ?? "", still_granted_by: str(r.still_granted_by), column: str(r.column), wanted: r.wanted === true }];
  });
}

/** The schema-changes refusals the editor answers in its own words; anything else is the server's message. */
export function failureOf(err: unknown): Failure {
  if (err instanceof ApiError) {
    if (err.code === "privileges_changed_since_read") {
      const objects = Array.isArray(err.extra.objects) ? err.extra.objects.filter(isTarget) : [];
      return { kind: "stale", objects };
    }
    if (err.code === "privileges_not_applied") return { kind: "not_applied", mismatches: mismatchesOf(err.extra.mismatches), message: err.message };
    if (err.code === "exposes_data_api") {
      const chain = Array.isArray(err.extra.chain) ? err.extra.chain.filter((c): c is string => typeof c === "string") : [];
      return { kind: "exposes", chain, opIndex: typeof err.extra.op_index === "number" ? err.extra.op_index : null, message: err.message };
    }
  }
  return { kind: "error", message: errorText(err) };
}

/** The read's whole schema list and default privileges (the schema-less page, followed to its end). */
async function readHead(databaseId: string, branchId: string, pg: string, role: string, signal: AbortSignal): Promise<PrivilegesPage> {
  let page = await getPrivileges(databaseId, branchId, pg, { role }, { signal });
  const head = { ...page, schemas: [...page.schemas], default_privileges: [...page.default_privileges] };
  for (let guard = 0; page.next_after && guard < 50; guard++) {
    page = await getPrivileges(databaseId, branchId, pg, { role, after: page.next_after }, { signal });
    head.schemas.push(...page.schemas);
    for (const d of page.default_privileges) {
      if (!head.default_privileges.some((x) => x.for_role === d.for_role && x.schema === d.schema && x.object === d.object)) head.default_privileges.push(d);
    }
  }
  return { ...head, next_after: null };
}

/**
 * The privilege editor's state (H5, R5): the role's read (schemas first, each schema's objects read
 * when it is first opened, paged), the staged edits, and the review → apply flow with `expect_acl`
 * (P10). Owner runs only (R6): the request never names a role to act as.
 */
export function usePrivilegeEditor({ databaseId, branchId, pg, role, enabled }: { databaseId: string; branchId: string; pg: string; role: string; enabled: boolean }) {
  const queryClient = useQueryClient();
  const keyOf = useCallback(
    (schema?: string, after?: string) =>
      getGetPrivilegesQueryKey(databaseId, branchId, pg, { role, ...(schema === undefined ? {} : { schema }), ...(after === undefined ? {} : { after }) }),
    [databaseId, branchId, pg, role],
  );
  const head = useQuery({
    queryKey: keyOf(),
    enabled,
    queryFn: ({ signal }) => readHead(databaseId, branchId, pg, role, signal),
  });
  const schemaNames = useMemo(() => head.data?.schemas.map((s) => s.name) ?? [], [head.data]);

  // `opened`: what is shown open (the first schema until the user says otherwise); `fetched`: every
  // schema ever opened, kept read so its staged edits stay checkable after it is closed.
  const [opened, setOpened] = useState<ReadonlySet<string> | null>(null);
  const [fetched, setFetched] = useState<ReadonlySet<string>>(new Set());
  const [wanted, setWanted] = useState<Record<string, number>>({});
  const open = opened ?? new Set(schemaNames.slice(0, 1));
  const reading = new Set([...fetched, ...open]);

  const pages: { schema: string; after?: string }[] = [];
  for (const schema of schemaNames) {
    if (!reading.has(schema)) continue;
    let after: string | undefined;
    for (let i = 0; i < (wanted[schema] ?? 1); i++) {
      pages.push({ schema, after });
      const data = queryClient.getQueryData<PrivilegesPage>(keyOf(schema, after));
      if (!data?.next_after) break;
      after = data.next_after;
    }
  }
  const results = useQueries({
    queries: pages.map(({ schema, after }) => ({
      queryKey: keyOf(schema, after),
      enabled,
      queryFn: ({ signal }: { signal: AbortSignal }) =>
        getPrivileges(databaseId, branchId, pg, { role, schema, ...(after === undefined ? {} : { after }) }, { signal }),
    })),
  });

  const bySchema = new Map<string, SchemaRead>();
  const allObjects: ObjectPrivileges[] = [];
  pages.forEach(({ schema }, i) => {
    const r = results[i];
    const entry = bySchema.get(schema) ?? { objects: [], more: false, loading: false, error: null };
    if (r.data) {
      entry.objects = [...entry.objects, ...r.data.objects];
      allObjects.push(...r.data.objects);
      entry.more = r.data.next_after != null;
    } else {
      entry.more = false;
      entry.loading ||= r.isPending;
      entry.error ??= r.error;
    }
    bySchema.set(schema, entry);
  });
  // `bySchema` and the index change with every read; the index is rebuilt each render (cheap).
  const index: PrivilegeIndex = indexPrivileges(head.data, allObjects);

  const [edits, setEdits] = useState<ReadonlyMap<string, Edit>>(new Map());
  const [review, setReview] = useState<Review | null>(null);
  const [applied, setApplied] = useState<string | null>(null);
  const applying = useRef(false);

  /** The staged edits that still change something against the read, in staging order. */
  const pending = [...edits.values()].filter(
    (e) =>
      stillChanges(index, e) &&
      // A revoke of the privilege takes its grant option with it.
      !(e.kind === "option" && [...edits.values()].some((r) => r.kind === "privilege" && !r.grant && r.column === null && r.privilege === e.privilege && JSON.stringify(r.target) === JSON.stringify(e.target))),
  );

  function stage(next: Edit[]) {
    setApplied(null);
    setEdits((prev) => {
      const map = new Map(prev);
      for (const e of next) {
        if (stillChanges(index, e)) map.set(editKey(e), e);
        else map.delete(editKey(e));
      }
      return map;
    });
  }
  const unstage = (e: Edit) =>
    setEdits((prev) => {
      const map = new Map(prev);
      map.delete(editKey(e));
      return map;
    });
  const staged = (e: Edit): Edit | undefined => edits.get(editKey(e));

  async function startReview() {
    const list = pending;
    if (list.length === 0) return;
    const body: SchemaChangeRequest = {
      dry_run: true,
      database: pg,
      operations: list.map((e) => operationOf(e, role)),
      expect_acl: expectAclOf(list, index),
    };
    setApplied(null);
    setReview({ body, edits: list, running: true, applying: false, response: null, failure: null });
    try {
      const response = await applySchemaChanges(databaseId, branchId, body);
      setReview((r) => (r && r.body === body ? { ...r, running: false, response } : r));
    } catch (err) {
      const failure = failureOf(err);
      setReview((r) => (r && r.body === body ? { ...r, running: false, failure } : r));
      if (failure.kind === "exposes" && failure.opIndex !== null && list.length > 1 && failure.opIndex < list.length) {
        // Keep the evidence: dry-run the others without the refused change (never applied).
        const refused = failure.opIndex;
        const rest = list.filter((_, i) => i !== refused);
        const passed = list.map((_, i) => i).filter((i) => i !== refused);
        try {
          const response = await applySchemaChanges(databaseId, branchId, {
            dry_run: true,
            database: pg,
            operations: rest.map((e) => operationOf(e, role)),
            expect_acl: expectAclOf(rest, index),
          });
          if (response.would_succeed !== false) setReview((r) => (r && r.body === body ? { ...r, partial: { response, passed } } : r));
        } catch {
          // The refusal alone stands.
        }
      }
    }
  }

  async function apply() {
    const r = review;
    if (!r || applying.current || r.running || r.failure || !r.response) return;
    applying.current = true;
    setReview({ ...r, applying: true });
    try {
      await applySchemaChanges(databaseId, branchId, { ...r.body, dry_run: false });
      setEdits(new Map());
      setReview(null);
      setApplied(`Applied ${r.edits.length} ${r.edits.length === 1 ? "change" : "changes"}.`);
      void queryClient.invalidateQueries({ queryKey: [keyOf()[0]] });
      void queryClient.invalidateQueries({ queryKey: ["members", "role-summary", databaseId, branchId] });
    } catch (err) {
      setReview((cur) => (cur && cur.body === r.body ? { ...cur, applying: false, failure: failureOf(err) } : cur));
    } finally {
      applying.current = false;
    }
  }

  /** Reads everything again (keeping the edits) and goes back to editing. */
  async function reload() {
    await queryClient.invalidateQueries({ queryKey: [keyOf()[0]] });
    setReview(null);
  }

  // Edits live in this tab only: leaving the page with some asks first.
  const dirty = edits.size > 0;
  useEffect(() => {
    if (!dirty) return;
    const onBeforeUnload = (e: BeforeUnloadEvent) => {
      e.preventDefault();
      e.returnValue = "";
    };
    window.addEventListener("beforeunload", onBeforeUnload);
    return () => window.removeEventListener("beforeunload", onBeforeUnload);
  }, [dirty]);

  return {
    head,
    schemaNames,
    isOpen: (schema: string) => open.has(schema),
    toggleSchema: (schema: string) => {
      const next = new Set(open);
      if (next.has(schema)) next.delete(schema);
      else next.add(schema);
      setOpened(next);
      setFetched((f) => (f.has(schema) ? f : new Set([...f, schema])));
    },
    loadMore: (schema: string) => setWanted((w) => ({ ...w, [schema]: (w[schema] ?? 1) + 1 })),
    schemaRead: (schema: string): SchemaRead | undefined => bySchema.get(schema),
    index,
    pending,
    stage,
    unstage,
    staged,
    discard: () => setEdits(new Map()),
    review,
    startReview,
    back: () => setReview(null),
    apply,
    reload,
    applied,
  };
}

export type PrivilegeEditorState = ReturnType<typeof usePrivilegeEditor>;
