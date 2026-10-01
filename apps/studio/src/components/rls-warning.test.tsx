import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { DataApiSchema, Privilege as Priv, TableDesc as DataApiTable } from "@cheladb/management-client";
import { RlsWarning } from "./rls-warning";

type Privs = { anon?: Priv[]; authenticated?: Priv[] };
type Kind = DataApiTable["kind"];
const table = (rls: boolean, privs: Privs = {}, kind: Kind = "table", securityInvoker = false): DataApiTable => ({
  kind,
  rls,
  security_invoker: securityInvoker,
  privileges: { anon: privs.anon ?? [], authenticated: privs.authenticated ?? [] },
  columns: {},
});
const schemaOf = (schemas: Record<string, Record<string, DataApiTable>>): DataApiSchema => ({
  version: "1",
  default_schema: "public",
  schemas: Object.fromEntries(Object.entries(schemas).map(([n, tables]) => [n, { tables, functions: {}, enums: {} }])),
});
const one = (t: DataApiTable, name = "notes", schema = "public") => schemaOf({ [schema]: { [name]: t } });

afterEach(() => vi.unstubAllGlobals());

describe("RlsWarning", () => {
  it("lists only reachable relations that bypass RLS", () => {
    render(
      <RlsWarning
        schema={schemaOf({
          public: {
            notes: table(false, { anon: ["select"] }),
            guarded: table(true, { anon: ["select"] }),
            private: table(false),
            mine: table(false, { authenticated: ["select"] }),
            a_view: table(false, { anon: ["select"] }, "view"),
            invoker_view: table(false, { anon: ["select"] }, "view", true),
            hidden_view: table(false, {}, "view"),
          },
          extra: { logs: table(false, { authenticated: ["select"] }) },
        })}
      />,
    );
    const items = screen.getAllByRole("listitem").map((li) => li.getAttribute("data-table"));
    expect(items.sort()).toEqual(["extra.logs", "public.a_view", "public.mine", "public.notes"]);
  });

  it("says the rows can be read, then to enable RLS before production", () => {
    render(<RlsWarning schema={one(table(false, { anon: ["select"] }))} />);
    expect(screen.getByText("Anyone with your public key can read these rows. Enable RLS before production.")).toBeInTheDocument();
    expect(screen.queryByText(/and change them/)).not.toBeInTheDocument();
  });

  it("adds 'and change them' when anon can insert, update or delete", () => {
    for (const priv of ["insert", "update", "delete"] as const) {
      const { unmount } = render(<RlsWarning schema={one(table(false, { anon: ["select", priv] }))} />);
      expect(
        screen.getByText("Anyone with your public key can read these rows and change them. Enable RLS before production."),
      ).toBeInTheDocument();
      unmount();
    }
  });

  it("doesn't say 'and change them' for writes only authenticated has", () => {
    render(<RlsWarning schema={one(table(false, { anon: ["select"], authenticated: ["insert"] }))} />);
    expect(screen.queryByText(/and change them/)).not.toBeInTheDocument();
  });

  it("names the authenticated role, not the public key, when only authenticated can reach the items", () => {
    render(<RlsWarning schema={one(table(false, { authenticated: ["select"] }))} />);
    expect(
      screen.getByText("Anyone with a secret key for the authenticated role can read these rows. Enable RLS before production."),
    ).toBeInTheDocument();
    expect(screen.queryByText(/public key/)).not.toBeInTheDocument();
  });

  it("shows both lines when anon and authenticated-only items are mixed", () => {
    render(
      <RlsWarning
        schema={schemaOf({
          public: { a: table(false, { anon: ["select"] }), b: table(false, { authenticated: ["select", "update"] }) },
        })}
      />,
    );
    expect(screen.getByText(/Anyone with your public key can read these rows\./)).toBeInTheDocument();
    expect(
      screen.getByText(/Anyone with a secret key for the authenticated role can read these rows and change them\./),
    ).toBeInTheDocument();
  });

  it("explains that RLS without a policy hides every row", () => {
    render(<RlsWarning schema={one(table(false, { anon: ["select"] }))} />);
    expect(
      screen.getByText("With RLS on and no policy, anon and authenticated can't see any rows."),
    ).toBeInTheDocument();
  });

  it("shows a copyable ALTER TABLE per table", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    render(<RlsWarning schema={one(table(false, { anon: ["select"] }))} />);
    const sql = 'ALTER TABLE "public"."notes" ENABLE ROW LEVEL SECURITY;';
    expect(screen.getByText(sql)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Copy ALTER TABLE for public.notes" }));
    expect(writeText).toHaveBeenCalledWith(sql);
  });

  it("explains a view that runs with its owner's rights and offers security_invoker", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    render(<RlsWarning schema={one(table(false, { anon: ["select"] }, "view"), "v")} />);
    expect(
      screen.getByText("Runs with its owner's rights, so RLS on the tables it reads doesn't apply."),
    ).toBeInTheDocument();
    const sql = 'ALTER VIEW "public"."v" SET (security_invoker = true);';
    expect(screen.getByText(sql)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Copy ALTER VIEW for public.v" }));
    expect(writeText).toHaveBeenCalledWith(sql);
  });

  it("suggests revoking a materialized view, which can't have RLS", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    render(<RlsWarning schema={one(table(false, { anon: ["select"] }, "materialized_view"), "m")} />);
    expect(screen.getByText("A materialized view can't have RLS.")).toBeInTheDocument();
    const sql = 'REVOKE ALL ON "public"."m" FROM anon, authenticated;';
    expect(screen.getByText(sql)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Copy REVOKE for public.m" }));
    expect(writeText).toHaveBeenCalledWith(sql);
    expect(screen.queryByText(/ENABLE ROW LEVEL SECURITY/)).not.toBeInTheDocument();
  });

  it("suggests revoking a foreign table, which can't have RLS", () => {
    render(<RlsWarning schema={one(table(false, { authenticated: ["select"] }, "foreign_table"), "f")} />);
    expect(screen.getByText("A foreign table can't have RLS.")).toBeInTheDocument();
    expect(screen.getByText('REVOKE ALL ON "public"."f" FROM anon, authenticated;')).toBeInTheDocument();
    expect(screen.queryByText(/ENABLE ROW LEVEL SECURITY/)).not.toBeInTheDocument();
  });

  it("doesn't list a security_invoker view, or a relation nobody can reach", () => {
    const { container } = render(
      <RlsWarning
        schema={schemaOf({
          public: {
            v: table(false, { anon: ["select"] }, "view", true),
            m: table(false, {}, "materialized_view"),
            f: table(false, {}, "foreign_table"),
          },
        })}
      />,
    );
    expect(container).toBeEmptyDOMElement();
  });

  it("quotes odd names, doubling any double quote", () => {
    render(<RlsWarning schema={one(table(false, { anon: ["select"] }), "my table", 'we"ird')} />);
    expect(
      screen.getByText('ALTER TABLE "we""ird"."my table" ENABLE ROW LEVEL SECURITY;'),
    ).toBeInTheDocument();
    render(<RlsWarning schema={one(table(false, { anon: ["select"] }, "view"), 'v"', 's"')} />);
    expect(screen.getByText('ALTER VIEW "s"""."v""" SET (security_invoker = true);')).toBeInTheDocument();
  });

  it("reads the privilege map as the API sends it: a missing role, or no map, is no access", () => {
    const anonOnly: DataApiTable = { kind: "table", rls: false, security_invoker: false, privileges: { anon: ["select"] }, columns: {} };
    const authOnly: DataApiTable = { ...anonOnly, privileges: { authenticated: ["update"] } };
    const noMap: DataApiTable = { kind: "table", rls: false, security_invoker: false, columns: {} };
    render(<RlsWarning schema={schemaOf({ public: { a: anonOnly, b: authOnly, c: noMap, d: { ...noMap, privileges: null } } })} />);
    const items = screen.getAllByRole("listitem").map((li) => li.getAttribute("data-table"));
    expect(items.sort()).toEqual(["public.a", "public.b"]);
    expect(
      screen.getByText("Anyone with a secret key for the authenticated role can read these rows and change them. Enable RLS before production."),
    ).toBeInTheDocument();
  });

  it("renders nothing when no reachable relation bypasses RLS", () => {
    const { container } = render(
      <RlsWarning schema={schemaOf({ public: { guarded: table(true, { anon: ["select"] }), private: table(false) } })} />,
    );
    expect(container).toBeEmptyDOMElement();
  });
});
