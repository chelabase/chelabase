// The checks in the order they run: the ones that change data come after the ones that only read,
// and the one that stalls the runtime comes last.
import type { Check } from "../lib.ts";
import { check as listsTwoTools } from "./lists-two-tools.ts";
import { check as searchFindsAnOperation } from "./search-finds-an-operation.ts";
import { check as executeChainsCalls } from "./execute-chains-calls.ts";
import { check as deleteNeedsTheFlag } from "./delete-needs-the-flag.ts";
import { check as viewerCannotWrite } from "./viewer-cannot-write.ts";
import { check as unknownRouteIsRefused } from "./unknown-route-is-refused.ts";
import { check as sandboxHasNoNetwork } from "./sandbox-has-no-network.ts";
import { check as readOnlyModeRefusesWrites } from "./read-only-mode-refuses-writes.ts";
import { check as sqlIsReadOnlyWithoutTheFlag } from "./sql-is-read-only-without-the-flag.ts";
import { check as largeResultIsTruncated } from "./large-result-is-truncated.ts";
import { check as timeoutThenRecovers } from "./timeout-then-recovers.ts";

export const checks: Check[] = [
  listsTwoTools,
  searchFindsAnOperation,
  executeChainsCalls,
  deleteNeedsTheFlag,
  viewerCannotWrite,
  unknownRouteIsRefused,
  sandboxHasNoNetwork,
  readOnlyModeRefusesWrites,
  sqlIsReadOnlyWithoutTheFlag,
  largeResultIsTruncated,
  timeoutThenRecovers,
];
