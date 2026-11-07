import { useEffect, useRef, useState } from "react";
import type { LiveStatus } from "@/components/table-editor/live-badge";
import { useRealtime } from "@/lib/realtime/context";
import { createRefetchGate } from "@/lib/realtime/live-rows";
import type { Key, StorageSubscription } from "@/lib/realtime/types";
import type { StorageItem } from "@/lib/storage-api";

/** Where the browser is: the branch's main Postgres database, the bucket, the Act-as role (null: owner mode). */
export interface LiveTarget {
  databaseId: string;
  branchId: string;
  pgDatabase: string;
  bucketId: string;
  role: string | null;
}

/** An object id as a storage channel's key: the key string of its one key column (S11). */
export function objectKey(id: string): Key {
  return JSON.stringify([id]);
}

/**
 * The listing's view keys: each file's id and each folder row's one object under it. Deleting a
 * folder's last object deletes that object, so its keyed delete reaches the viewer and the row
 * goes; other objects inside a folder change nothing on screen. Every id is one the role's own
 * listing returned, and realtime confirms each before it counts.
 */
export function viewIds(items: StorageItem[]): string[] {
  return items.flatMap((i) => (i.id ? [i.id] : []));
}

/**
 * Object storage m17: a bucket's live changes, as the role sees them. Realtime sends only keys of
 * objects the role can list in this bucket; the listing can't place an id in a folder, so any
 * change reads the listing again (its folder, search and sort), through the table editor's gate
 * (debounced, spaced, backing off). `joined`/`status` live reads it again at once (changes may have
 * happened while paused). `ids`: the rows on screen ({@link viewIds}), sent as view keys, so a delete of
 * one of them reaches this viewer. Returns the live state for the badge; null without realtime or before the
 * first answer.
 */
export function useLiveObjects(target: LiveTarget, ids: string[], refetch: () => void): LiveStatus | null {
  const realtime = useRealtime();
  const [status, setStatus] = useState<LiveStatus | null>(null);
  const latest = useRef(refetch);
  latest.current = refetch;
  const sub = useRef<StorageSubscription | null>(null);
  const sentView = useRef("");
  const keys = ids.map(objectKey);
  const keysRef = useRef(keys);
  keysRef.current = keys;
  const { databaseId, branchId, pgDatabase, bucketId, role } = target;

  useEffect(() => {
    if (!realtime) return;
    const gate = createRefetchGate(() => latest.current());
    const s = realtime.joinStorage(
      { database_id: databaseId, branch_id: branchId, pg_database: pgDatabase, bucket_id: bucketId, role },
      {
        onJoined: (info) => {
          setStatus({ live: info.live, ...(info.reason ? { reason: info.reason } : {}) });
          if (info.live) gate.runNow();
        },
        onStatus: (st) => {
          setStatus({ live: st.live, ...(st.reason ? { reason: st.reason } : {}) });
          if (st.live) gate.runNow();
        },
        // As the table editor: only a lost channel pauses; any other error keeps the state it finds.
        onError: (err) => setStatus((cur) => (err.code === "forbidden" || err.code === "not_found" || cur === null ? { live: false, reason: err.code } : cur)),
        onRows: () => gate.request(),
        onResync: () => gate.request(),
      },
    );
    sub.current = s;
    sentView.current = JSON.stringify(keysRef.current);
    if (keysRef.current.length > 0) s.view(keysRef.current);
    return () => {
      s.leave();
      gate.cancel();
      sub.current = null;
      sentView.current = "";
      setStatus(null);
    };
  }, [realtime, databaseId, branchId, pgDatabase, bucketId, role]);

  // R19: the files on screen.
  const text = JSON.stringify(keys);
  useEffect(() => {
    const s = sub.current;
    if (!s || text === sentView.current) return;
    sentView.current = text;
    s.view(JSON.parse(text) as Key[]);
  }, [text]);

  return status;
}
