import { act, renderHook, waitFor } from "@testing-library/react";
import { type ReactNode } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { beforeEach, describe, expect, it } from "vitest";
import { configureManagementClient } from "@chelabase/management-client";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { B, FULL, editorRoutes, meAs } from "@/test/table-editor-fixtures";
import { TableEditorStoreContext, createTableEditorStore, useTableEditor } from "./store";
import { usePlace } from "./use-place";

function setup(routes: ReturnType<typeof editorRoutes>) {
  configureManagementClient({ baseUrl: "http://studio.test", getToken: () => "t", onUnauthorized: () => false, fetchImpl: fakeFetch(routes) });
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const store = createTableEditorStore(null);
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={qc}>
      <TableEditorStoreContext.Provider value={store}>{children}</TableEditorStoreContext.Provider>
    </QueryClientProvider>
  );
  return wrapper;
}

beforeEach(() => { localStorage.clear(); sessionStorage.clear(); });

describe("usePlace", () => {
  it("shares_the_act_as_role_between_two_consumers_of_the_same_params", async () => {
    const wrapper = setup(editorRoutes());
    const { result } = renderHook(
      () => {
        const first = usePlace(FULL);
        const second = usePlace(FULL);
        const picked = useTableEditor((s) => s.actAs);
        return { first, second, picked };
      },
      { wrapper },
    );
    await waitFor(() => expect(result.current.first.actAsReady).toBe(true));
    expect(result.current.first.actAs).toBeNull(); // an Admin acts as the owner by default
    act(() => result.current.first.setActAs("billing_admin"));
    expect(result.current.first.actAs).toBe("billing_admin");
    expect(result.current.second.actAs).toBe("billing_admin");
    expect(result.current.picked["d1/b1"]).toBe("billing_admin");
  });

  it("reports_no_role_and_the_refusal_reason_when_nothing_is_grantable", async () => {
    const wrapper = setup(editorRoutes({ me: meAs("developer"), routes: { [`GET ${B}/roles`]: () => jsonResponse([]) } }));
    const { result } = renderHook(() => usePlace(FULL), { wrapper });
    expect(result.current.actAsReason).toBe("Loading roles…");
    await waitFor(() => expect(result.current.actAsReady).toBe(true));
    expect(result.current.actAs).toBeNull();
    expect(result.current.allowed.owner).toBe(false);
    expect(result.current.actAsReason).toBe("You can't act as any Postgres role here.");
  });
});
