import { vi } from "vitest";
import type { ApiClient } from "@/lib/api";

/** An ApiClient whose every call is a vi.fn(); override what a test needs. */
export function fakeApi(overrides: Partial<ApiClient> = {}): ApiClient {
  return {
    me: vi.fn(),
    createDatabase: vi.fn(),
    getDatabase: vi.fn(),
    getConnection: vi.fn(),
    resetPassword: vi.fn(),
    deleteDatabase: vi.fn(),
    updateDatabase: vi.fn(),
    listBranches: vi.fn(),
    createBranch: vi.fn(),
    getBranchConnection: vi.fn(),
    resetBranchPassword: vi.fn(),
    resetBranchToParent: vi.fn(),
    restoreBranch: vi.fn(),
    deleteBranch: vi.fn(),
    // The data API panel loads these as soon as a ready branch shows: empty by default.
    listKeys: vi.fn().mockResolvedValue([]),
    createKey: vi.fn(),
    rotateKey: vi.fn(),
    revokeKey: vi.fn(),
    getDataApiSchema: vi.fn().mockResolvedValue({ version: "0", default_schema: "public", schemas: {} }),
    ...overrides,
  };
}
