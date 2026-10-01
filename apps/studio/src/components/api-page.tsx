import { Link } from "@tanstack/react-router";
import { Suspense, lazy } from "react";

// Scalar and its stylesheet are large: load them only when this page opens.
const ApiReference = lazy(() => import("@/components/api-reference"));

export function ApiPage() {
  return (
    <div className="min-h-screen bg-background text-foreground">
      <header className="flex items-center justify-between border-b px-8 py-4">
        <span className="font-semibold">Cheladb Studio</span>
        <Link to="/app" className="text-sm underline-offset-4 hover:underline">
          Back to dashboard
        </Link>
      </header>
      <Suspense fallback={<p className="p-8 text-muted-foreground">Loading the API reference…</p>}>
        <ApiReference />
      </Suspense>
    </div>
  );
}
