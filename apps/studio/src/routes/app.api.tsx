import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { ApiPage } from "@/components/api-page";
import { parseApiSearch } from "@/lib/api-docs";

export const Route = createFileRoute("/app/api")({ validateSearch: parseApiSearch, component: ApiRoute });

function ApiRoute() {
  const { doc } = Route.useSearch();
  const navigate = useNavigate();
  return (
    <ApiPage
      doc={doc ?? "management"}
      onDocChange={(next) => void navigate({ to: "/app/api", search: next === "management" ? {} : { doc: next }, replace: true })}
    />
  );
}
