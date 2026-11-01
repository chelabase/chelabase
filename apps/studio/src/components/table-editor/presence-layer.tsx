import { Tooltip } from "@base-ui/react/tooltip";
import { useEffect, useRef, useState, type ReactElement } from "react";
import type { Key, PresenceMember } from "@/lib/realtime/types";
import { avatarLabels, avatarStack, displayName, drawnMembers, people, presenceColor } from "@/lib/realtime/presence";
import { usePresence } from "@/lib/realtime/presence-store";
import { useMediaQuery } from "@/lib/use-media-query";
import { cn } from "@/lib/utils";

export interface PresenceGeometry {
  rowHeight: number;
  /** The top of the first row inside the layer. */
  offsetTop: number;
  /** A column's box inside the layer; null when the column isn't shown. */
  columnBox(column: string): { left: number; width: number } | null;
}

export interface PresenceLayerProps {
  members: PresenceMember[];
  selfSub: string;
  /** The key of each row shown, in order (null: no key). */
  rowKeys: (Key | null)[];
  geometry: PresenceGeometry;
  /**
   * The viewer's own active cell as the grid knows it right now (null: none). It decides whether the viewer's outline
   * takes the outermost ring of a cell. Left out, the viewer's own presence entry decides (it lags by the debounce).
   */
  selfCell?: { row: Key; column: string } | null;
}

/**
 * How many 2px rings fit in a cell of `rowHeight`: 20px stays free and each ring takes 2px off every side (4px of
 * height), at least one. The viewer's own active outline counts as one of them.
 */
export function maxRings(rowHeight: number): number {
  return Number.isFinite(rowHeight) ? Math.max(1, Math.floor((rowHeight - 20) / 4)) : 1;
}

/** Plain string order (not locale order), so every client nests the same members in the same order. */
const byId = (a: { m: PresenceMember }, b: { m: PresenceMember }) => (a.m.id < b.m.id ? -1 : a.m.id > b.m.id ? 1 : 0);

/** The first row's hover tag keeps its side (below) and shifts to fit. */
const FIRST_ROW_COLLISIONS = { side: "shift", align: "shift" } as const;

/** A colour at a low alpha, for tints. */
const tint = (color: string, pct: number) => `color-mix(in oklab, ${color} ${pct}%, transparent)`;

/** What a hover tag says about one member: name, email and the role they act as. */
function TagBody({ member }: { member: PresenceMember }) {
  return (
    <div>
      <span className="flex items-center gap-1.5 font-medium">
        <span aria-hidden="true" className="size-2 shrink-0" style={{ background: presenceColor(member.color_seed) }} />
        {displayName(member)}
      </span>
      {member.email && member.email !== displayName(member) && <span className="block text-chalk-dim">{member.email}</span>}
      <span className="block text-chalk-dim">
        acting as <span className="font-mono">{member.role ?? "the owner"}</span>
      </span>
    </div>
  );
}

const POPUP_CLASS = "max-w-72 border border-line-strong bg-deep px-2 py-1.5 text-xs text-chalk";

/** The hover tag of a member, opened by hovering its trigger (the avatars). */
function MemberTag({ member, trigger }: { member: PresenceMember; trigger: ReactElement<Record<string, unknown>> }) {
  return (
    <Tooltip.Root>
      <Tooltip.Trigger delay={150} render={trigger} />
      <Tooltip.Portal>
        <Tooltip.Positioner side="top" sideOffset={6} className="z-50">
          <Tooltip.Popup role="tooltip" className={POPUP_CLASS}>
            <TagBody member={member} />
          </Tooltip.Popup>
        </Tooltip.Positioner>
      </Tooltip.Portal>
    </Tooltip.Root>
  );
}

/** The cell the pointer is over, as a row key and a column. */
interface HoverCell {
  row: Key;
  column: string;
}

/** One layer over the grid: other members' cell boxes, row tints and editing fills. */
export function PresenceLayer({ members, selfSub, rowKeys, geometry, selfCell }: PresenceLayerProps) {
  const reduced = useMediaQuery("(prefers-reduced-motion: reduce)");
  const layerRef = useRef<HTMLDivElement>(null);
  const [hover, setHover] = useState<HoverCell | null>(null);
  const [anchor, setAnchor] = useState<HTMLDivElement | null>(null);
  const drawn = drawnMembers(members, selfSub);
  const placed = drawn.flatMap((m) => {
    const index = rowKeys.indexOf(m.state.row);
    return index < 0 ? [] : [{ m, top: geometry.offsetTop + index * geometry.rowHeight, color: presenceColor(m.color_seed) }];
  });
  const move = reduced
    ? undefined
    : "top 120ms var(--ease-design), left 120ms var(--ease-design), width 120ms var(--ease-design), height 120ms var(--ease-design)";

  // Whether the viewer's own outline is on a cell: the grid's active cell when given, else the viewer's presence entry.
  const viewerOn = (row: Key, column: string) =>
    selfCell !== undefined
      ? selfCell !== null && selfCell.row === row && selfCell.column === column
      : members.some((m) => m.sub === selfSub && m.state.row === row && m.state.column === column);

  // The others on one cell nest by id: ring i sits 2i px in, below the viewer's own outline when that is there too,
  // and the members past the cap get no box (their row tint and their line in the hover tag stay).
  const cells = new Map<string, typeof placed>();
  for (const p of placed) {
    if (!p.m.state.column || !geometry.columnBox(p.m.state.column)) continue;
    const id = JSON.stringify([p.m.state.row, p.m.state.column]);
    cells.set(id, [...(cells.get(id) ?? []), p]);
  }
  const rings: { p: (typeof placed)[number]; ring: number; inset: number }[] = [];
  const cap = maxRings(geometry.rowHeight);
  for (const group of cells.values()) {
    const { row, column } = group[0]!.m.state;
    const base = viewerOn(row!, column!) ? 2 : 0;
    const room = cap - base / 2;
    [...group].sort(byId).forEach((p, ring) => {
      if (ring < room) rings.push({ p, ring, inset: base + 2 * ring });
    });
  }

  // The boxes take no pointer events (a click goes to the grid cell under them), so the grid's
  // pointer is mapped to a cell here instead: one listener on the layer's parent.
  const latest = useRef({ drawn, rowKeys, geometry });
  latest.current = { drawn, rowKeys, geometry };
  useEffect(() => {
    const layer = layerRef.current;
    const host = layer?.parentElement;
    if (!layer || !host) return;
    const onMove = (e: Event) => {
      const { clientX, clientY } = e as globalThis.MouseEvent;
      const rect = layer.getBoundingClientRect();
      const x = clientX - rect.left;
      const y = clientY - rect.top;
      const { drawn, rowKeys, geometry } = latest.current;
      const index = Math.floor((y - geometry.offsetTop) / geometry.rowHeight);
      const row = y >= geometry.offsetTop && index < rowKeys.length ? rowKeys[index] : null;
      let next: HoverCell | null = null;
      if (row != null) {
        for (const m of drawn) {
          const box = m.state.column ? geometry.columnBox(m.state.column) : null;
          if (m.state.column && box && x >= box.left && x < box.left + box.width) {
            next = { row, column: m.state.column };
            break;
          }
        }
      }
      setHover((cur) => (cur?.row === next?.row && cur?.column === next?.column ? cur : next));
    };
    const onLeave = () => setHover(null);
    const onOut = (e: Event) => {
      const to = (e as globalThis.MouseEvent).relatedTarget;
      if (!(to instanceof Node) || !host.contains(to)) setHover(null);
    };
    host.addEventListener("pointermove", onMove);
    host.addEventListener("mousemove", onMove);
    host.addEventListener("pointerleave", onLeave);
    host.addEventListener("mouseleave", onLeave);
    host.addEventListener("pointerout", onOut);
    host.addEventListener("mouseout", onOut);
    return () => {
      host.removeEventListener("pointermove", onMove);
      host.removeEventListener("mousemove", onMove);
      host.removeEventListener("pointerleave", onLeave);
      host.removeEventListener("mouseleave", onLeave);
      host.removeEventListener("pointerout", onOut);
      host.removeEventListener("mouseout", onOut);
    };
  }, []);

  // Everyone drawn on the hovered cell (gone when their row scrolls out of `rowKeys`).
  const here = hover
    ? placed.filter(({ m }) => m.state.row === hover.row && m.state.column === hover.column && geometry.columnBox(hover.column))
    : [];
  const hoverRow = hover && here.length > 0 ? rowKeys.indexOf(hover.row) : -1;
  const hoverBox = hover && hoverRow >= 0 ? geometry.columnBox(hover.column) : null;
  // The first row shown has the table header right above it: its tag opens below the cell instead.
  const side = hoverRow === 0 ? "bottom" : "top";

  return (
    <div
      ref={layerRef}
      data-testid="presence-layer"
      data-presence-layer=""
      data-reduced-motion={reduced ? "true" : "false"}
      aria-hidden="true"
      className="absolute inset-0 z-[5] overflow-hidden"
      style={{ pointerEvents: "none" }}
    >
      {placed.map(({ m, top, color }) => (
        <div
          key={`row-${m.id}`}
          data-testid="presence-row"
          data-row={m.state.row ?? undefined}
          data-color={color}
          className="absolute inset-x-0"
          style={{ top, height: geometry.rowHeight, background: tint(color, 7), transition: move }}
        />
      ))}
      {rings.map(({ p: { m, top, color }, ring, inset }) => {
        const box = geometry.columnBox(m.state.column!)!;
        return (
          <div
            key={`cell-${m.id}`}
            data-testid="presence-cell"
            data-member={m.id}
            data-row={m.state.row ?? undefined}
            data-column={m.state.column ?? undefined}
            data-color={color}
            data-editing={m.state.editing ? "true" : "false"}
            data-ring={ring}
            data-inset={inset}
            className="absolute"
            style={{
              top: top + inset,
              left: box.left + inset,
              width: Math.max(0, box.width - 2 * inset),
              height: Math.max(0, geometry.rowHeight - 2 * inset),
              pointerEvents: "none",
              boxShadow: `inset 0 0 0 2px ${color}`,
              background: m.state.editing ? tint(color, 18) : undefined,
              transition: move,
            }}
          />
        );
      })}
      {hoverBox && (
        // What the hover tag points at: the hovered cell itself, which has a box even when nobody's ring is drawn.
        <div
          ref={setAnchor}
          data-testid="presence-anchor"
          className="absolute"
          style={{ top: geometry.offsetTop + hoverRow * geometry.rowHeight, left: hoverBox.left, width: hoverBox.width, height: geometry.rowHeight, pointerEvents: "none" }}
        />
      )}
      <Tooltip.Root open={hoverBox != null && anchor != null}>
        <Tooltip.Portal>
          <Tooltip.Positioner
            anchor={anchor}
            side={side}
            sideOffset={6}
            // Below the first row, flipping would put the tag back over the header: it shifts to fit instead.
            collisionAvoidance={side === "bottom" ? FIRST_ROW_COLLISIONS : undefined}
            className="z-50"
          >
            <Tooltip.Popup role="tooltip" className={cn(POPUP_CLASS, "flex flex-col gap-1.5")}>
              {here.map(({ m }) => (
                <TagBody key={m.id} member={m} />
              ))}
            </Tooltip.Popup>
          </Tooltip.Positioner>
        </Tooltip.Portal>
      </Tooltip.Root>
    </div>
  );
}

/** The avatar stack of the table header: one avatar per member (not per socket), the viewer (`selfSub`) left out. */
export function AvatarStack({ members, selfSub = "" }: { members: PresenceMember[]; selfSub?: string }) {
  const list = people(members, selfSub);
  if (list.length === 0) return null;
  const { shown, extra } = avatarStack(list);
  const labels = avatarLabels(shown);
  return (
    <div data-testid="avatar-stack" className="flex items-center" aria-label={`${list.length} ${list.length === 1 ? "person" : "people"} here`} role="group">
      {shown.map((m, i) => {
        const color = presenceColor(m.color_seed);
        return (
          <MemberTag
            key={m.id}
            member={m}
            trigger={
              <span
                data-testid="avatar"
                data-color={color}
                tabIndex={0}
                aria-label={`${displayName(m)}, acting as ${m.role ?? "the owner"}`}
                className={cn(
                  "flex size-6 items-center justify-center border border-ink font-sans text-[10px] font-semibold text-ink outline-none select-none focus-visible:outline-2 focus-visible:outline-sand",
                  i > 0 && "-ml-1.5",
                )}
                style={{ background: color }}
              >
                {labels[i]}
              </span>
            }
          />
        );
      })}
      {extra > 0 && <span className="ml-1.5 font-sans text-xs text-chalk-dim tabular-nums">+{extra}</span>}
    </div>
  );
}

/** The `PresenceLayer` of the open table: its members come from the table's presence store. */
export function TablePresenceLayer(props: Omit<PresenceLayerProps, "members">) {
  const members = usePresence((s) => s.members);
  return <PresenceLayer members={members} {...props} />;
}

/** The `AvatarStack` of the open table: its members come from the table's presence store. */
export function TableAvatarStack({ selfSub }: { selfSub?: string }) {
  const members = usePresence((s) => s.members);
  return <AvatarStack members={members} selfSub={selfSub} />;
}
