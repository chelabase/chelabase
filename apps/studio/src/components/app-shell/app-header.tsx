import { Menu } from "@base-ui/react/menu";
import { useQueryClient } from "@tanstack/react-query";
import { CircleUser, LogOut } from "lucide-react";

export interface AppHeaderProps {
  profile: { name?: string; email?: string };
  onSignOut: () => void;
}

const focusRing =
  "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-sand";

/** The top bar of the signed-in shell: an empty context slot and the profile menu. */
export function AppHeader({ profile, onSignOut }: AppHeaderProps) {
  const queryClient = useQueryClient();
  const label = profile.name || profile.email;
  return (
    <header className="sticky top-0 z-20 flex h-[52px] shrink-0 items-center justify-between border-b border-line bg-background px-4">
      <div data-slot="context" />
      <Menu.Root>
        <Menu.Trigger
          aria-label={label ? undefined : "Account"}
          className={`flex h-9 items-center gap-2 px-2 text-sm hover:bg-chalk/[0.08] data-[popup-open]:bg-chalk/[0.08] ${focusRing}`}
        >
          <CircleUser aria-hidden="true" size={18} strokeWidth={1.5} />
          {label && <span>{label}</span>}
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
