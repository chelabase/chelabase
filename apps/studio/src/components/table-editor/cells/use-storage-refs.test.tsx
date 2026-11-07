import { configureManagementClient } from "@chelabase/management-client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { describe, expect, it } from "vitest";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import type { RefRequest } from "./storage-ref";
import { useStorageRefs, type RefScope } from "./use-storage-refs";

const RESOLVE = "POST /v1/databases/d1/branches/b1/storage/object/resolve";
const B = "3f2b8c1e-7d6a-4e59-9b0c-2a1d4e5f6a7b";
const signed = (name: string) => `/storage/v1/object/sign/avatars/${name}?token=t`;

type Body = { refs: RefRequest[]; expires_in?: number };

/** A resolve route that signs every ref whose name `visible` accepts, as the role the query names. */
function resolver(visible: (name: string, role: string | null) => boolean) {
  return fakeFetch({
    [RESOLVE]: async (req) => {
      const role = new URL(req.url).searchParams.get("role");
      const body = (await req.clone().json()) as Body;
      return jsonResponse({
        urls: body.refs.map((r) => {
          const name = "name" in r ? r.name : r.id;
          return visible(name, role) ? signed(name) : null;
        }),
        expires_at: 1,
      });
    },
  });
}

function setup(f: ReturnType<typeof fakeFetch>, scope: Partial<RefScope>, requests: RefRequest[]) {
  configureManagementClient({ baseUrl: "http://studio.test", getToken: () => "t", onUnauthorized: () => false, fetchImpl: f });
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const wrapper = ({ children }: { children: ReactNode }) => <QueryClientProvider client={qc}>{children}</QueryClientProvider>;
  const full: RefScope = { databaseId: "d1", branchId: "b1", database: null, role: null, apiUrl: "https://api.test", admin: true, ...scope };
  return renderHook(() => useStorageRefs(full, requests, true), { wrapper });
}
const bodies = (f: ReturnType<typeof fakeFetch>) => Promise.all(f.calls.map((r) => r.clone().json() as Promise<Body>));

describe("useStorageRefs", () => {
  // #106 AC1: a page of 50 rows with refs makes one resolve request (each distinct ref once).
  it("a_page_of_50_rows_resolves_in_one_request", async () => {
    const f = resolver(() => true);
    const requests: RefRequest[] = Array.from({ length: 50 }, (_, i) => ({ bucket_id: B, name: `u/${i % 40}.png` }));
    const { result } = setup(f, {}, requests);
    await waitFor(() => expect(result.current.get(requests[49])).toBeDefined());
    expect(f.calls).toHaveLength(1);
    const [body] = await bodies(f);
    expect(body.refs).toHaveLength(40);
    expect(result.current.get({ bucket_id: B, name: "u/3.png" })).toEqual({
      state: "ok",
      url: "https://api.test/storage/v1/object/sign/avatars/u/3.png?token=t",
      bucket: "avatars",
      name: "u/3.png",
      public: false,
    });
  });

  it("nothing_to_resolve_asks_nothing", async () => {
    const f = resolver(() => true);
    const { result } = setup(f, {}, []);
    expect(result.current.get({ id: B })).toBeUndefined();
    expect(f.calls).toHaveLength(0);
  });

  // In owner mode nothing is hidden: a null is a missing object.
  it("as_the_owner_a_null_is_missing", async () => {
    const f = resolver((name) => name !== "gone.png");
    const { result } = setup(f, { role: null }, [{ bucket: "avatars", name: "gone.png" }]);
    await waitFor(() => expect(result.current.get({ bucket: "avatars", name: "gone.png" })).toEqual({ state: "missing" }));
    expect(f.calls).toHaveLength(1);
  });

  // #106 AC4/AC5: an Admin acting as a role tells hidden from missing with one owner-mode follow-up for the nulls.
  it("an_admin_acting_as_a_role_tells_no_access_from_missing", async () => {
    const f = resolver((name, role) => (name === "gone.png" ? false : name === "secret.png" ? role === null : true));
    const refs: RefRequest[] = [
      { bucket: "avatars", name: "ok.png" },
      { bucket: "avatars", name: "secret.png" },
      { bucket: "avatars", name: "gone.png" },
    ];
    const { result } = setup(f, { role: "authenticated", admin: true }, refs);
    await waitFor(() => expect(result.current.get(refs[2])).toBeDefined());
    expect(result.current.get(refs[0])?.state).toBe("ok");
    expect(result.current.get(refs[1])).toEqual({ state: "no_access" });
    expect(result.current.get(refs[2])).toEqual({ state: "missing" });
    expect(f.calls).toHaveLength(2);
    expect(new URL(f.calls[0].url).searchParams.get("role")).toBe("authenticated");
    expect(new URL(f.calls[1].url).searchParams.get("role")).toBeNull();
    expect((await bodies(f))[1].refs).toEqual([refs[1], refs[2]]);
  });

  // Below Admin there is no owner mode: a null can't be told apart and reads as no access.
  it("below_admin_a_null_is_no_access_without_a_follow_up", async () => {
    const f = resolver(() => false);
    const ref: RefRequest = { id: B };
    const { result } = setup(f, { role: "app_reader", admin: false }, [ref]);
    await waitFor(() => expect(result.current.get(ref)).toEqual({ state: "no_access" }));
    expect(f.calls).toHaveLength(1);
  });

  it("sends_the_postgres_database_when_it_is_not_the_main_one", async () => {
    const f = resolver(() => true);
    const ref: RefRequest = { id: B };
    const { result } = setup(f, { database: "analytics" }, [ref]);
    await waitFor(() => expect(result.current.get(ref)).toBeDefined());
    expect(new URL(f.calls[0].url).searchParams.get("database")).toBe("analytics");
  });
});
