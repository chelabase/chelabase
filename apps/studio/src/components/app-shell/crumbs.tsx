import { Menu } from "@base-ui/react/menu";
import { Link, useRouterState } from "@tanstack/react-router";
import { ChevronRight, Ellipsis } from "lucide-react";
import { createContext, Fragment, type ComponentPropsWithoutRef, useCallback, useContext, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { neededTrailWidth } from "@/lib/crumb-fit";
import { buildCrumbs, type Crumb, type CrumbLabel, type CrumbLabels, type CrumbParams } from "@/lib/crumbs";
import { TABLES_ROUTE } from "./rail-items";
import { NARROW_QUERY, useMediaQuery } from "@/lib/use-media-query";

type Register = (param: string, entry: CrumbLabel) => () => void;

// Two contexts (S8): pages only read the stable `register`, so a registration never re-renders them.
const LabelsContext = createContext<CrumbLabels>({});
const RegisterContext = createContext<Register | null>(null);

/** Holds the labels pages supply for dynamic route segments. Mounted once in `AppShell`. */
export function CrumbLabelsProvider({ children }: { children: ReactNode }) {
  const [labels, setLabels] = useState<CrumbLabels>({});
  const register = useCallback<Register>((param, entry) => {
    setLabels((prev) => ({ ...prev, [param]: entry }));
    return () =>
      setLabels((prev) => {
        if (prev[param] !== entry) return prev;
        const { [param]: _gone, ...rest } = prev;
        return rest;
      });
  }, []);
  return (
    <RegisterContext.Provider value={register}>
      <LabelsContext.Provider value={labels}>{children}</LabelsContext.Provider>
    </RegisterContext.Provider>
  );
}

/**
 * Supplies the crumb label for a dynamic segment (`projectId`, `databaseId`, `memberId`, `role`, `table`).
 * The label carries the id it belongs to, so it shows only while that id is the current param.
 * An undefined or empty id or label registers nothing. A no-op without a provider.
 */
export function useCrumbLabel(param: string, id: string | undefined, label: string | undefined): void {
  const register = useContext(RegisterContext);
  useEffect(() => {
    if (!register || !id || !label) return;
    return register(param, { id, label });
  }, [register, param, id, label]);
}

const focusRing = "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-sand";
const linkClass = `hover:text-chalk ${focusRing}`;

function Chevron() {
  return <ChevronRight aria-hidden="true" size={14} strokeWidth={1.5} className="shrink-0" />;
}

function Loading() {
  return (
    <>
      <span aria-hidden="true">…</span>
      <span className="sr-only">Loading</span>
    </>
  );
}

/** Crumbs whose labels are fixed words: they keep their width. Dynamic labels (names, ids) shrink and truncate. */
const STATIC_KEYS = new Set(["projects", "members", "roles", "tables"]);

/** Identifiers (a role, a table) are lettered as code. */
const MONO_KEYS = new Set(["role", "table"]);

const TABLE_OPTIONAL = ["projectId", "databaseId", "branchId", "pgDatabase", "schema", "table"];

/** A crumb's router link. Extra props (Base UI's menu item props) pass through to the anchor. */
function CrumbLink({ crumb, ...rest }: { crumb: Crumb } & Omit<ComponentPropsWithoutRef<"a">, "href">) {
  let params: Record<string, string | undefined> | undefined = crumb.params;
  if (crumb.to === TABLES_ROUTE) {
    // Optional segments left out would be inherited from the current location: clear them explicitly.
    params = { ...Object.fromEntries(TABLE_OPTIONAL.map((k) => [k, undefined])), ...crumb.params };
  }
  return (
    <Link to={crumb.to as never} params={params as never} search={crumb.search as never} activeOptions={{ exact: true }} title={crumb.label} {...rest} />
  );
}

function CrumbItem({ crumb, last }: { crumb: Crumb; last: boolean }) {
  if (last) {
    return (
      <span aria-current="page" title={crumb.loading ? undefined : crumb.label} className={`min-w-0 truncate text-chalk ${MONO_KEYS.has(crumb.key) ? "font-mono" : ""}`}>
        {crumb.loading ? <Loading /> : crumb.label}
      </span>
    );
  }
  if (crumb.to && !crumb.loading) {
    return (
      <CrumbLink crumb={crumb} className={`min-w-0 truncate ${linkClass} ${MONO_KEYS.has(crumb.key) ? "font-mono" : ""}`}>
        {crumb.label}
      </CrumbLink>
    );
  }
  return (
    <span title={crumb.loading ? undefined : crumb.label} className={`min-w-0 truncate ${MONO_KEYS.has(crumb.key) ? "font-mono" : ""}`}>
      {crumb.loading ? <Loading /> : crumb.label}
    </span>
  );
}

const menuItemClass =
  "flex cursor-default items-center px-2 py-1.5 outline-none data-[highlighted]:bg-chalk/[0.08] data-[highlighted]:outline-solid data-[highlighted]:outline-2 data-[highlighted]:-outline-offset-2 data-[highlighted]:outline-sand";

function HiddenCrumbs({ crumbs }: { crumbs: Crumb[] }) {
  return (
    <Menu.Root>
      <Menu.Trigger
        aria-label="Show hidden breadcrumbs"
        className={`flex h-7 shrink-0 items-center px-1.5 hover:bg-chalk/[0.08] data-[popup-open]:bg-chalk/[0.08] ${focusRing}`}
      >
        <Ellipsis aria-hidden="true" size={16} strokeWidth={1.5} />
      </Menu.Trigger>
      <Menu.Portal>
        <Menu.Positioner align="start" sideOffset={6} className="z-50">
          <Menu.Popup className="min-w-40 max-w-[calc(100vw-2rem)] border border-line-strong bg-deep p-1 text-sm text-chalk">
            {crumbs.map((c) =>
              c.to && !c.loading ? (
                <Menu.LinkItem key={c.key} render={<CrumbLink crumb={c} />} className={menuItemClass}>
                  <span className="truncate">{c.label}</span>
                </Menu.LinkItem>
              ) : (
                <Menu.Item key={c.key} disabled aria-disabled="true" className={menuItemClass}>
                  {c.loading ? <Loading /> : <span className="truncate">{c.label}</span>}
                </Menu.Item>
              ),
            )}
          </Menu.Popup>
        </Menu.Positioner>
      </Menu.Portal>
    </Menu.Root>
  );
}

/** The header trail, built from the route. Below the rail breakpoint a trail over two crumbs that doesn't fit collapses its middle (Q3). */
export function Breadcrumbs() {
  // Both come from the deepest match, so they always belong to the same router commit.
  const pathname = useRouterState({ select: (s) => s.matches[s.matches.length - 1]?.pathname ?? s.location.pathname });
  const params = useRouterState({ select: (s) => s.matches[s.matches.length - 1]?.params }) as CrumbParams | undefined;
  const labels = useContext(LabelsContext);
  const narrow = useMediaQuery(NARROW_QUERY);
  const crumbs = buildCrumbs(pathname, params ?? {}, labels);

  const navRef = useRef<HTMLElement>(null);
  const [collapsed, setCollapsed] = useState(false);
  // The width the whole trail needs, known once it has been seen not to fit.
  const needed = useRef<number | null>(null);
  const signature = crumbs.map((c) => `${c.key}:${c.label}:${c.loading ? 1 : 0}`).join("\u0000");
  const eligible = narrow && crumbs.length > 2;

  // A new trail is measured whole first.
  useLayoutEffect(() => {
    needed.current = null;
    setCollapsed(false);
  }, [signature, narrow]);

  useLayoutEffect(() => {
    const nav = navRef.current;
    if (!eligible || !nav) return;
    const measure = () => {
      if (collapsed) {
        if (needed.current !== null && nav.clientWidth >= needed.current) setCollapsed(false);
        return;
      }
      const truncating = Array.from(nav.querySelectorAll<HTMLElement>(".truncate"));
      const width = neededTrailWidth(
        nav.clientWidth,
        truncating.map((el) => ({ scroll: el.scrollWidth, client: el.clientWidth, last: el.getAttribute("aria-current") === "page" })),
      );
      if (width > nav.clientWidth) {
        needed.current = width;
        setCollapsed(true);
      }
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(nav);
    return () => observer.disconnect();
  }, [eligible, collapsed, signature]);

  if (crumbs.length === 0) return null;

  const collapse = eligible && collapsed;
  const last = crumbs.length - 1;
  return (
    <nav ref={navRef} aria-label="Breadcrumb" className="min-w-0 shrink text-sm text-chalk-dim max-[40rem]:flex-1">
      <ol className="flex min-w-0 items-center gap-1.5">
        {crumbs.map((c, i) => {
          if (collapse && i > 0 && i < last) {
            if (i !== 1) return null;
            return (
              <Fragment key="hidden">
                <li className="flex shrink-0 items-center gap-1.5">
                  <Chevron />
                  <HiddenCrumbs crumbs={crumbs.slice(1, last)} />
                </li>
              </Fragment>
            );
          }
          return (
            <li key={c.key} className={`flex items-center gap-1.5 ${i === last ? "min-w-0 shrink-[20] max-[40rem]:min-w-[calc(8rem+1.25rem)]" : !STATIC_KEYS.has(c.key) ? "min-w-0" : "min-w-0 shrink-0"}`}>
              {i > 0 && <Chevron />}
              <CrumbItem crumb={c} last={i === last} />
            </li>
          );
        })}
      </ol>
    </nav>
  );
}
