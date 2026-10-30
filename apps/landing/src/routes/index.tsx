import { createFileRoute } from '@tanstack/react-router';
import { LandingPage } from '@/components/landing-page';

// Resolved at build time: the prerendered HTML carries the docs links.
const docsUrl = import.meta.env.VITE_DOCS_URL as string | undefined;

export const Route = createFileRoute('/')({
  component: () => <LandingPage docsUrl={docsUrl} />,
});
