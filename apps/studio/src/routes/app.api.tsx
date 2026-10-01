import { createFileRoute } from "@tanstack/react-router";
import { ApiPage } from "@/components/api-page";

export const Route = createFileRoute("/app/api")({ component: ApiPage });
