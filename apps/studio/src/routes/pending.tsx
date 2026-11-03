import { createFileRoute } from "@tanstack/react-router";
import { PendingNotice } from "@/components/login/pending-notice";

export const Route = createFileRoute("/pending")({ component: PendingNotice });
