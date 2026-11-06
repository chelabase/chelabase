import { Menu } from "@base-ui/react/menu";
import { useQueryClient } from "@tanstack/react-query";
import { CircleUser, LogOut, Menu as MenuIcon } from "lucide-react";
import type { RefObject } from "react";
import { Breadcrumbs } from "./crumbs";

export interface AppHeaderProps {
  profile: { name?: string; email?: string };
  onSignOut: () => void;
  /** Narrow widths: given, the header starts with a menu button that calls it. */
  onMenu?: () => void;
  menuOpen?: boolean;
  /** The drawer popup's id; `aria-controls` points at it while the drawer is open. */
  menuControlsId?: string;
  menuButtonRef?: RefObject<HTMLButtonElement | null>;
}

const ALPHA_NOTICE = "Chelabase is early, pre-release software and not ready for production data.";

const focusRing =
  "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-sand";

/** The top bar of the signed-in shell: the breadcrumbs, a context slot (`#app-header-context`, filled by `HeaderContext`), the alpha tag and the profile menu. */
export function AppHeader({ profile, onSignOut, onMenu, menuOpen = false, menuControlsId, menuButtonRef }: AppHeaderProps) {
  const queryClient = useQueryClient();
  const label = profile.name || profile.email;
  return (
    <header className="sticky top-0 z-20 flex min-h-[52px] shrink-0 items-center border-b border-line bg-background px-4 sm:has-[#app-header-context>*]:[&_nav]:max-w-[10rem] max-[40rem]:flex-wrap">
      <div className="flex h-[52px] min-w-0 items-center max-[40rem]:flex-1">
      {onMenu && (
        <button
          ref={menuButtonRef}
          type="button"
          aria-label="Menu"
          aria-haspopup="dialog"
          aria-expanded={menuOpen}
          aria-controls={menuOpen ? menuControlsId : undefined}
          onClick={onMenu}
          className={`-ml-2 mr-1 flex size-10 shrink-0 items-center justify-center hover:bg-chalk/[0.08] ${focusRing}`}
        >
          <MenuIcon aria-hidden="true" size={20} strokeWidth={1.5} />
        </button>
      )}
      <Breadcrumbs />
      </div>
      {/* Wide: beside the crumbs. Below the breakpoint: a full-width second row (P5, amended). */}
      <div
        id="app-header-context"
        data-slot="context"
        className="flex min-w-0 flex-1 items-center px-3 sm:has-[>*]:min-w-[20rem] max-[40rem]:order-last max-[40rem]:-mx-4 max-[40rem]:basis-[calc(100%+2rem)] max-[40rem]:overflow-x-auto max-[40rem]:border-t max-[40rem]:border-line max-[40rem]:px-4 max-[40rem]:py-2 max-[40rem]:empty:hidden"
      />
      {/* Every signed-in screen says it: the same notice the README, docs and landing page carry. */}
      <span
        title={ALPHA_NOTICE}
        className="mr-2 shrink-0 cursor-default border border-sand/60 px-1.5 py-px text-xs font-medium uppercase tracking-wide text-sand"
      >
        Alpha
        <span className="sr-only">: {ALPHA_NOTICE}</span>
      </span>
      <Menu.Root>
        <Menu.Trigger
          aria-label={label ? undefined : "Account"}
          className={`flex h-9 shrink-0 items-center gap-2 px-2 text-sm hover:bg-chalk/[0.08] data-[popup-open]:bg-chalk/[0.08] ${focusRing}`}
        >
          <CircleUser aria-hidden="true" size={18} strokeWidth={1.5} />
          {label && <span className="max-[40rem]:sr-only">{label}</span>}
        </Menu.Trigger>
        <Menu.Portal>
          <Menu.Positioner align="end" sideOffset={6} className="z-50">
            <Menu.Popup className="min-w-48 border border-line-strong bg-deep p-1 text-sm text-chalk">
              {profile.email && (
                <div className="px-2 py-1.5 text-xs opacity-80">{profile.email}</div>
              )}
              <Menu.Item
                className="flex cursor-default items-center gap-2 px-2 py-1.5 outline-none data-[highlighted]:bg-chalk/[0.08] data-[highlighted]:outline-solid data-[highlighted]:outline-2 data-[highlighted]:-outline-offset-2 data-[highlighted]:outline-sand"
                onClick={() => {
                  // Nothing of this session may outlive it in the cache.
                  queryClient.clear();
                  onSignOut();
                }}
              >
                <LogOut aria-hidden="true" size={16} strokeWidth={1.5} />
                Sign out
              </Menu.Item>
            </Menu.Popup>
          </Menu.Positioner>
        </Menu.Portal>
      </Menu.Root>
    </header>
  );
}
