import { act, renderHook } from "@testing-library/react";
import type { ReactNode } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { configureManagementClient } from "@chelabase/management-client";
import { fakeFetch } from "@/test/fake-fetch";
import { B, schemaChanges } from "@/test/table-editor-fixtures";
import { newEdit, type Edit } from "@/lib/table-editor/changeset";
import { useDryRun } from "./use-dry-run";

const T = { schema: "public", name: "invoices" };
const add = (name: string) => newEdit([{ add_column: { table: T, column: { name, data_type: { schema: "pg_catalog", name: "int8" } } } }]);

describe("useDryRun", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());
  it("debounces_400ms_and_posts_the_ops", async () => {
    const f = fakeFetch({ [`POST ${B}/schema-changes`]: schemaChanges });
    configureManagementClient({ baseUrl: "http://studio.test", getToken: () => "t", onUnauthorized: () => false, fetchImpl: f });
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const wrapper = ({ children }: { children: ReactNode }) => <QueryClientProvider client={qc}>{children}</QueryClientProvider>;
    const props = (edits: Edit[]) => ({ databaseId: "d1", branchId: "b1", database: null, role: "billing_admin", edits, table: "public.invoices", totalBytes: 100, enabled: true });
    const { result, rerender } = renderHook((p: ReturnType<typeof props>) => useDryRun(p), { wrapper, initialProps: props([add("a")]) });
    await act(async () => { vi.advanceTimersByTime(200); });
    rerender(props([add("a"), add("b")]));
    await act(async () => { vi.advanceTimersByTime(399); });
    expect(f.calls).toHaveLength(0);
    await act(async () => { vi.advanceTimersByTime(1); });
    await vi.waitFor(() => expect(result.current.view?.apply).toBe("ready"));
    expect(f.calls).toHaveLength(1);
    const body = await f.calls[0].clone().json();
    expect(body).toMatchObject({ dry_run: true, role: "billing_admin", database: null });
    expect(body.operations).toHaveLength(2);
  });
});
