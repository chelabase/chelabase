import { createFileRoute } from "@tanstack/react-router";
import { ZITADEL_VERSION } from "@cheladb/zitadel";
import { Button } from "@/components/ui/button";
import { useStudioStore } from "@/stores/studio-store";

export const Route = createFileRoute("/")({ component: Home });

function Home() {
  const ready = useStudioStore((s) => s.ready);

  return (
    <div className="min-h-screen bg-background p-8 text-foreground">
      <h1 className="text-3xl font-semibold tracking-tight">Cheladb Studio</h1>
      <p className="mt-3 max-w-xl text-muted-foreground">
        Scaffold placeholder. Routes and product UI come next. Workspace packages
        are wired: <code className="text-sm">@cheladb/types</code>,{" "}
        <code className="text-sm">@cheladb/zitadel</code> ({ZITADEL_VERSION}).
      </p>
      <p className="mt-4 text-sm text-muted-foreground">
        Store ready: {ready ? "yes" : "no"}
      </p>
      <div className="mt-6">
        <Button type="button">shadcn / Base UI</Button>
      </div>
    </div>
  );
}
