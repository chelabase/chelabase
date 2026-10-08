import { createFileRoute } from '@tanstack/react-router';
import { LandingPage } from '@/components/landing-page';
import { studioUrl } from '@/studio-url';

// Resolved at build time: the prerendered HTML carries the Studio links.
const studio = studioUrl(import.meta.env.VITE_STUDIO_URL);

export const Route = createFileRoute('/')({
  component: () => <LandingPage studioUrl={studio} />,
});
