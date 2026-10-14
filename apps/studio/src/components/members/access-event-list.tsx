import { useEffect, useRef, useState, type RefObject } from "react";
import { useInfiniteQuery, type QueryKey } from "@tanstack/react-query";
import { ChevronRight } from "lucide-react";
import type { AccessEvent, AccessEventPage } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { eventFields, eventSentence, relativeTime, utc, type EventField, type SentenceContext } from "@/lib/members";
import { cn } from "@/lib/utils";

export interface AccessEventListProps {
  /** The infinite query's key (one per list: the tenant's, or a project's). */
  queryKey: QueryKey;
  /** Loads one page, newest first; `before` is the previous page's `next_before`. */
  fetchPage: (before: number | undefined) => Promise<AccessEventPage>;
  context: SentenceContext;
  emptyText: string;
}

/**
 * Access changes as sentences, newest first, 50 at a time ("Show older" appends). Each row can
 * open to show the event's `before` and `after` as two mono blocks. Shared with the project's
 * Access tab.
 */
export function AccessEventList({ queryKey, fetchPage, context, emptyText }: AccessEventListProps) {
  const events = useInfiniteQuery({
    queryKey,
    queryFn: ({ pageParam }) => fetchPage(pageParam),
    initialPageParam: undefined as number | undefined,
    getNextPageParam: (last) => last.next_before ?? undefined,
  });
  // "Show older" goes away with the last page: focus moves to the first entry it brought
  // (its details button), never to the page body.
  const pendingFrom = useRef<number | null>(null);
  const [focusId, setFocusId] = useState<number | null>(null);
  const focusTarget = useRef<HTMLButtonElement>(null);
  const loaded = events.data?.pages.flatMap((p) => p.events) ?? [];
  useEffect(() => {
    if (pendingFrom.current === null || events.isFetchingNextPage) return;
    const from = pendingFrom.current;
    pendingFrom.current = null;
    if (!events.hasNextPage && loaded.length > from) setFocusId(loaded[from].id);
  }, [events.isFetchingNextPage, events.hasNextPage, loaded]);
  useEffect(() => {
    if (focusId !== null) focusTarget.current?.focus();
  }, [focusId]);

  if (events.isPending) return <p className="text-sm text-chalk-dim">Loading activity…</p>;
  if (events.isError && !events.data) {
    return (
      <div className="space-y-3">
        <p role="alert" className="text-sm text-coral">
          Couldn't load activity: {errorText(events.error)}
        </p>
        <Button variant="outline" onClick={() => void events.refetch()}>
          Retry
        </Button>
      </div>
    );
  }
  const all = events.data.pages.flatMap((p) => p.events);
  if (all.length === 0) return <p className="max-w-[65ch] text-sm text-chalk-dim">{emptyText}</p>;
  return (
    <div className="space-y-3">
      <ol className="border-y border-line">
        {all.map((e) => (
          <EventRow key={e.id} event={e} context={context} detailsRef={e.id === focusId ? focusTarget : undefined} />
        ))}
      </ol>
      {events.isFetchNextPageError && (
        <p role="alert" className="text-sm text-coral">
          Couldn't load older activity: {errorText(events.error)}
        </p>
      )}
      {events.hasNextPage && (
        <Button
          variant="outline"
          size="sm"
          disabled={events.isFetchingNextPage}
          onClick={() => {
            // The first entry the page brings takes focus if the button goes (the last page).
            setFocusId(null);
            pendingFrom.current = all.length;
            void events.fetchNextPage();
          }}
        >
          {events.isFetchingNextPage ? "Loading…" : "Show older"}
        </Button>
      )}
    </div>
  );
}

function EventRow({ event, context, detailsRef }: { event: AccessEvent; context: SentenceContext; detailsRef?: RefObject<HTMLButtonElement | null> }) {
  const [open, setOpen] = useState(false);
  const detailsId = `event-${event.id}`;
  return (
    <li className="border-b border-line last:border-b-0">
      <div className="flex min-h-9 items-center gap-3 text-sm">
        <time dateTime={event.at} title={utc(event.at)} className="w-28 shrink-0 text-chalk-dim tabular-nums">
          {relativeTime(event.at)}
        </time>
        <span className="min-w-0 flex-1 text-chalk">{eventSentence(event, context)}</span>
        <button
          ref={detailsRef}
          type="button"
          aria-expanded={open}
          aria-controls={detailsId}
          aria-label={open ? "Hide details" : "Show details"}
          onClick={() => setOpen((o) => !o)}
          className="flex size-7 items-center justify-center text-chalk-dim outline-none hover:bg-chalk/[0.06] hover:text-chalk focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand"
        >
          <ChevronRight aria-hidden="true" size={16} strokeWidth={1.5} className={cn("transition-transform duration-150 motion-reduce:transition-none", open && "rotate-90")} />
        </button>
      </div>
      {open && (
        <div id={detailsId} className="grid gap-3 pb-3 sm:grid-cols-2">
          <Payload label="Before" fields={eventFields(event.before, context, event.action)} />
          <Payload label="After" fields={eventFields(event.after, context, event.action)} />
        </div>
      )}
    </li>
  );
}

/** One side of an event as labelled fields (names, not ids; words, not JSON). */
function Payload({ label, fields }: { label: string; fields: EventField[] }) {
  return (
    <div className="min-w-0 space-y-1">
      <div className="text-xs text-chalk-dim">{label}</div>
      {fields.length === 0 ? (
        <p className="border border-line bg-deep p-2 text-xs text-chalk-dim">—</p>
      ) : (
        <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 border border-line bg-deep p-2 text-xs">
          {fields.map((f) => (
            <div key={f.label} className="contents">
              <dt className="text-chalk-dim">{f.label}</dt>
              <dd className="min-w-0 break-words text-chalk">{f.value}</dd>
            </div>
          ))}
        </dl>
      )}
    </div>
  );
}
