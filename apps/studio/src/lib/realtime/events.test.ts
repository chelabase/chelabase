import { getGetMeQueryKey, getListBranchDatabasesQueryKey, getListBranchesQueryKey } from "@chelabase/management-client";
import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";
import { SLOW_POLL_MS, fallbackInterval, invalidateAll, invalidateForEvent } from "./events";
import type { ProjectEvent } from "./types";

function spyClient() {
  const qc = new QueryClient();
  const spy = vi.spyOn(qc, "invalidateQueries").mockResolvedValue();
  const keys = () => spy.mock.calls.map((c) => (c[0] as { queryKey: unknown }).queryKey);
  return { qc, spy, keys };
}

const BRANCHES = getListBranchesQueryKey("d1");
const DATABASES = getListBranchDatabasesQueryKey("d1", "b1");
const ME = getGetMeQueryKey();

describe("project events invalidate exactly the orval keys (§5.3, P10)", () => {
  it.each<[ProjectEvent, unknown[]]>([
    [{ event: "branch.status", database_id: "d1", branch_id: "b1", status: "ready" }, [BRANCHES, DATABASES]],
    [{ event: "branch.created", database_id: "d1", branch_id: "b1" }, [BRANCHES, DATABASES]],
    [{ event: "branch.deleted", database_id: "d1", branch_id: "b1" }, [BRANCHES, DATABASES]],
    [{ event: "database.created", database_id: "d1" }, [ME]],
    [{ event: "database.deleted", database_id: "d1" }, [ME]],
    [{ event: "pg_database.created", database_id: "d1", branch_id: "b1", name: "reporting" }, [DATABASES]],
    [{ event: "pg_database.dropped", database_id: "d1", branch_id: "b1", name: "reporting" }, [DATABASES]],
  ])("%j", (event, expected) => {
    const { qc, keys } = spyClient();
    invalidateForEvent(qc, event);
    // Exactly these keys, built by the generated helpers (no hand-built arrays), in any order.
    expect(keys()).toHaveLength(expected.length);
    expect(keys()).toEqual(expect.arrayContaining(expected));
  });

  it("an_event_for_another_database_touches_that_databases_keys_only", () => {
    const { qc, keys } = spyClient();
    invalidateForEvent(qc, { event: "branch.status", database_id: "d9", branch_id: "b7", status: "ready" });
    expect(keys()).toEqual(expect.arrayContaining([getListBranchesQueryKey("d9"), getListBranchDatabasesQueryKey("d9", "b7")]));
    expect(keys()).not.toContainEqual(BRANCHES);
  });

  it("invalidateAll_covers_the_branch_list_the_branch_databases_and_me", () => {
    const { qc, keys } = spyClient();
    invalidateAll(qc, { databaseId: "d1", branchId: "b1" });
    expect(keys()).toEqual(expect.arrayContaining([BRANCHES, DATABASES, ME]));
  });
});

describe("polling fallback (P10)", () => {
  it("the_slow_poll_is_30_s", () => {
    expect(SLOW_POLL_MS).toBe(30_000);
  });

  it("while_live_the_interval_is_the_slow_poll_even_when_a_branch_is_busy", () => {
    expect(fallbackInterval(true, false)).toBe(30_000);
    expect(fallbackInterval(true, 2000)).toBe(30_000);
  });

  it("when_not_live_todays_polling_returns", () => {
    expect(fallbackInterval(false, 2000)).toBe(2000);
    expect(fallbackInterval(false, false)).toBe(false);
  });
});
