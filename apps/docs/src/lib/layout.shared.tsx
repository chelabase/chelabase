import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';
import { appName } from './shared';

// No GitHub link: the repository is private.
export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: appName,
    },
  };
}
