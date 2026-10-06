import { Suspense, lazy } from "react";

// Scalar and its stylesheet are large: load them only when this page opens.
const ApiReference = lazy(() => import("@/components/api-reference"));

export function ApiPage() {
  return (
    <Suspense fallback={<p className="p-8 text-muted-foreground">Loading the API reference…</p>}>
      <ApiReference />
    </Suspense>
  );
}
