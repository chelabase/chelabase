import type { Table } from "@chelabase/management-client";

const col = (attnum: number, name: string, data_type: string, type_oid: number, over: Record<string, unknown> = {}) => ({
  attnum, name, data_type, type_oid, not_null: false, default: null, comment: null, collation: null, generated: null, identity: null, grants: [], ...over,
});

/** `public.invoices`: id uuid PK, customer_id FK, status enum, amount_cents, memo. */
export function invoices(over: Partial<Table> = {}): Table {
  return {
    schema: "public", name: "invoices", oid: 16600, owner: { name: "app_owner", oid: 10 }, comment: null,
    rls_enabled: true, rls_forced: false, unlogged: false, options: [], grants: [], indexes: [], policies: [],
    columns: [
      col(1, "id", "uuid", 2950, { not_null: true, default: "gen_random_uuid()" }),
      col(2, "customer_id", "uuid", 2950, { not_null: true }),
      col(3, "status", "invoice_status", 16500, { not_null: true, default: "'open'::invoice_status" }),
      col(4, "amount_cents", "bigint", 20, { not_null: true }),
      col(5, "memo", "text", 25),
    ],
    constraints: [
      { name: "invoices_pkey", kind: "primary_key", columns: [1], definition: "PRIMARY KEY (id)", deferrable: false, initially_deferred: false, oid: 1, references: null, validated: true },
      { name: "invoices_customer_id_fkey", kind: "foreign_key", columns: [2], definition: "FOREIGN KEY (customer_id) REFERENCES customers(id)", deferrable: false, initially_deferred: false, oid: 2, references: { schema: "public", table: "customers", table_oid: 16610, columns: [1] }, validated: true },
    ],
    ...over,
  } as unknown as Table;
}

export const CATALOG = [
  { oid: 20, array_oid: 1016, schema: "pg_catalog", name: "int8", kind: "base", category: "N", preferred: false, takes_modifiers: false },
  { oid: 25, array_oid: 1009, schema: "pg_catalog", name: "text", kind: "base", category: "S", preferred: true, takes_modifiers: false },
  { oid: 16, array_oid: 1000, schema: "pg_catalog", name: "bool", kind: "base", category: "B", preferred: true, takes_modifiers: false },
  { oid: 2950, array_oid: 2951, schema: "pg_catalog", name: "uuid", kind: "base", category: "U", preferred: false, takes_modifiers: false },
  { oid: 1184, array_oid: 1185, schema: "pg_catalog", name: "timestamptz", kind: "base", category: "D", preferred: true, takes_modifiers: true },
  { oid: 16500, array_oid: 16499, schema: "public", name: "invoice_status", kind: "enum", category: "E", preferred: false, takes_modifiers: false, labels: ["open", "paid", "void"] },
] as const;
