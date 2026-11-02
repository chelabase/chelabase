import { Suspense, lazy } from "react";
import { Tabs } from "@/components/ui/tabs";
import { API_DOCS, type ApiDocId } from "@/lib/api-docs";

// Scalar and its stylesheet are large: load them only when this page opens.
const ApiReference = lazy(() => import("@/components/api-reference"));

export function ApiPage({ doc, onDocChange }: { doc: ApiDocId; onDocChange: (doc: ApiDocId) => void }) {
  return (
    <div className="pt-4">
      <div className="px-4 sm:px-8">
        <Tabs label="APIs" items={API_DOCS} value={doc} onValueChange={onDocChange}>
          {null}
        </Tabs>
      </div>
      <Suspense fallback={<p className="p-8 text-muted-foreground">Loading the API reference…</p>}>
        <ApiReference doc={doc} />
      </Suspense>
    </div>
  );
}
