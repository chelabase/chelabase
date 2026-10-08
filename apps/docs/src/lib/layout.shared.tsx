import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';
import { appName } from './shared';

// No GitHub link: the repository is private. No theme switch: the docs are dark only.
export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      // The text name in Spectral until the claw-C wordmark arrives with apps/landing.
      title: <span className="font-serif font-bold">{appName}</span>,
      // `/` only redirects here.
      url: '/docs',
    },
    themeSwitch: { enabled: false },
  };
}
