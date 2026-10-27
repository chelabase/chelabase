import { Tooltip } from "@base-ui/react/tooltip";
import { useLinkProps, useRouterState } from "@tanstack/react-router";
import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ComponentPropsWithRef,
  type FocusEvent,
  type KeyboardEvent,
  type ReactNode,
} from "react";
import { ClawMark, Wordmark } from "@chelabase/lagoon/crab-marks";
import type { RailBehaviour } from "@/lib/rail-preference";
import { cn } from "@/lib/utils";
import { RailBehaviourMenu } from "./rail-behaviour-menu";
import { RailItem } from "./rail-item";
import { RAIL_ENTRIES, type RailBadges } from "./rail-items";

/** Hover mode: how long the pointer must rest on the rail before it widens. */
export const HOVER_OPEN_DELAY_MS = 150;
/** Hover mode: how long the rail stays wide after the pointer leaves (re-entry cancels). */
export const HOVER_CLOSE_DELAY_MS = 200;

export interface AppRailProps {
  behaviour: RailBehaviour;
  onBehaviourChange: (next: RailBehaviour) => void;
  /** Counts on rail items (Members: sign-up requests waiting). */
  badges?: RailBadges;
  /** `drawer`: the overlay below the narrow breakpoint. Always expanded with labels, ignores `behaviour`, no setting menu. */
  variant?: "docked" | "drawer";
  /** Called when any link, the home link included, is clicked. */
  onNavigate?: () => void;
}

/**
 * The brand mark, linking to `/app`. It never carries `aria-current`: Projects is the current item
 * there. `<Link>` forces the attribute whenever its route matches, so this keeps TanStack's
 * navigation (href, click handling, preload) via `useLinkProps` and drops the active-state props.
 */
function HomeLink({ children, onNavigate }: { children: ReactNode; onNavigate?: () => void }) {
  const { "aria-current": _ariaCurrent, "data-status": _dataStatus, ...props } = useLinkProps({
    to: "/app",
    activeProps: {},
  }) as ComponentPropsWithRef<"a"> & { "data-status"?: string };
  return (
    <a
      {...props}
      aria-label="Chelabase Studio home"
      onClick={(e) => {
        (props as ComponentPropsWithRef<"a">).onClick?.(e);
        onNavigate?.();
      }}
      className="flex h-[52px] shrink-0 items-center px-[14px] text-chalk focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand"
    >
      {children}
    </a>
  );
}

/**
 * The shell's left rail. Expanded and collapsed are pinned; in hover mode the
 * rail keeps a 56px slot in the grid and its panel widens over the page when the
 * pointer rests on it, focus enters it, or its behaviour menu is open.
 */
export function AppRail({ behaviour: chosen, onBehaviourChange, badges, variant = "docked", onNavigate }: AppRailProps) {
  const drawer = variant === "drawer";
  // The drawer always shows labels, whatever the stored setting (which it leaves untouched).
  const behaviour: RailBehaviour = drawer ? "expanded" : chosen;
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const navRef = useRef<HTMLElement>(null);
  const openTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const closeTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const [pointerOpen, setPointerOpen] = useState(false);
  const [focusWithin, setFocusWithin] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);
  const [dismissed, setDismissed] = useState(false);
  /** Whether the pointer is physically over the rail, tracked in every mode. */
  const pointerInside = useRef(false);
  /** Set by a pointer press (in the rail or its portalled menu) and consumed by the next focus in the rail. */
  const pointerPressed = useRef(false);

  const hover = behaviour === "hover";
  // The menu popup is portalled: entering it reads as leaving the rail, so an open menu holds the rail open.
  const expanded = behaviour === "expanded" || (hover && (menuOpen || (!dismissed && (pointerOpen || focusWithin))));

  function clearTimers() {
    clearTimeout(openTimer.current);
    clearTimeout(closeTimer.current);
    openTimer.current = undefined;
    closeTimer.current = undefined;
  }

  // A behaviour change starts hover mode afresh from where the pointer really is.
  useEffect(() => {
    setPointerOpen(pointerInside.current);
    setDismissed(false);
    return clearTimers;
  }, [behaviour]);

  // Any key press means the next focus is the keyboard's, even after a press that focused nothing.
  useEffect(() => {
    const onKeyDown = () => {
      pointerPressed.current = false;
    };
    document.addEventListener("keydown", onKeyDown, true);
    return () => document.removeEventListener("keydown", onKeyDown, true);
  }, []);

  // Native enter/leave rather than React's: React's follow the component tree, so moving into the
  // portalled menu wouldn't count as leaving and the rail could stay open after the menu closes.
  const pointerHandlers = useRef({ enter: () => {}, leave: () => {} });
  useLayoutEffect(() => {
    pointerHandlers.current = { enter: handlePointerEnter, leave: handlePointerLeave };
  });
  useEffect(() => {
    const nav = navRef.current;
    if (!nav) return;
    const enter = () => pointerHandlers.current.enter();
    const leave = () => pointerHandlers.current.leave();
    nav.addEventListener("pointerenter", enter);
    nav.addEventListener("pointerleave", leave);
    return () => {
      nav.removeEventListener("pointerenter", enter);
      nav.removeEventListener("pointerleave", leave);
    };
  }, []);

  /** React events bubble through portals; only count those whose target is in the rail's own DOM. */
  function inRail(node: EventTarget | null): boolean {
    return node instanceof Node && navRef.current?.contains(node) === true;
  }

  function handlePointerEnter() {
    pointerInside.current = true;
    if (!hover) return;
    clearTimeout(closeTimer.current);
    closeTimer.current = undefined;
    setDismissed(false);
    if (pointerOpen || openTimer.current !== undefined) return;
    openTimer.current = setTimeout(() => {
      openTimer.current = undefined;
      setPointerOpen(true);
    }, HOVER_OPEN_DELAY_MS);
  }

  function handlePointerLeave() {
    pointerInside.current = false;
    if (!hover) {
      setPointerOpen(false);
      return;
    }
    clearTimeout(openTimer.current);
    openTimer.current = undefined;
    clearTimeout(closeTimer.current);
    closeTimer.current = setTimeout(() => {
      closeTimer.current = undefined;
      setPointerOpen(false);
    }, HOVER_CLOSE_DELAY_MS);
  }

  function handleFocus(event: FocusEvent) {
    if (!inRail(event.target)) return;
    // Only keyboard focus opens the rail: focus from a click (or Base UI handing focus back to the
    // menu trigger after a mouse pick) mustn't pin it open once the pointer leaves.
    const fromPointer = pointerPressed.current;
    pointerPressed.current = false;
    if (fromPointer) return;
    // Escape hides the labels for the focus it was pressed on; the next focus (from outside or
    // another rail item) brings them back.
    setDismissed(false);
    setFocusWithin(true);
  }

  function handleBlur(event: FocusEvent) {
    if (!inRail(event.target) || inRail(event.relatedTarget)) return;
    setFocusWithin(false);
  }

  function handleKeyDown(event: KeyboardEvent) {
    // Escape inside the open menu closes the menu only.
    if (event.key !== "Escape" || !hover || menuOpen || !inRail(event.target)) return;
    clearTimers();
    setPointerOpen(false);
    setDismissed(true);
  }

  function handleMenuOpenChange(open: boolean) {
    setMenuOpen(open);
    if (open) setDismissed(false);
  }

  const items = (
    <ul className="flex flex-col gap-1 py-2">
      {RAIL_ENTRIES.map((entry) => (
        <RailItem
          key={entry.id}
          entry={entry}
          pathname={pathname}
          expanded={expanded}
          tooltip={behaviour === "collapsed"}
          onNavigate={onNavigate}
          badge={entry.kind === "link" ? badges?.[entry.id] : undefined}
        />
      ))}
    </ul>
  );

  return (
    <nav
      ref={navRef}
      aria-label="Studio"
      className={cn("app-rail relative h-full shrink-0", hover && "w-14")}
      data-variant={variant}
      data-behaviour={behaviour}
      data-expanded={expanded ? "true" : "false"}
      onPointerDown={() => {
        pointerPressed.current = true;
      }}
      onFocus={handleFocus}
      onBlur={handleBlur}
      onKeyDown={handleKeyDown}
    >
      <div
        className={cn(
          "flex h-full flex-col overflow-hidden border-r border-line-strong bg-deep text-chalk",
          "transition-[width] duration-200 ease-lagoon motion-reduce:transition-none",
          drawer ? "w-full" : expanded ? "w-[232px]" : "w-14",
          hover && "absolute top-0 left-0 z-30",
        )}
      >
        <HomeLink onNavigate={onNavigate}>
          {expanded ? (
            <Wordmark className="font-serif text-xl whitespace-nowrap" />
          ) : (
            <ClawMark size={28} className="shrink-0" />
          )}
        </HomeLink>
        <div className="min-h-0 flex-1 overflow-y-auto overflow-x-hidden">
          {behaviour === "collapsed" ? <Tooltip.Provider delay={300}>{items}</Tooltip.Provider> : items}
        </div>
        {!drawer && (
          <div className="shrink-0 border-t border-line py-2">
            <RailBehaviourMenu
              behaviour={chosen}
              onBehaviourChange={onBehaviourChange}
              onOpenChange={handleMenuOpenChange}
              labelsVisible={expanded}
            />
          </div>
        )}
      </div>
    </nav>
  );
}
