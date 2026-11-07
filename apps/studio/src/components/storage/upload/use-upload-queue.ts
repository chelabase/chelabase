import { useCallback, useEffect, useReducer, useRef } from "react";
import { uploadKey, type DroppedFile } from "./drop-entries";
import { describeUploadError } from "./errors";
import { EMPTY_QUEUE, hasActiveUploads, nextToStart, queueReducer, type ConflictChoice, type UploadItem } from "./queue-reducer";
import { runUpload, type UploadApi } from "./runner";

export interface UploadQueueOptions {
  api: UploadApi;
  /** A file finished: the listing should be read again (called at most once a second, trailing). */
  onUploaded: () => void;
}

/** The listing refreshes at most this often while files finish. */
const REFRESH_EVERY = 1000;

/**
 * The upload queue's effects over its reducer: starts queued files up to the limit, runs each
 * with `runUpload`, aborts a file's request when it's paused or cancelled (and a cancelled
 * multipart upload's session), refreshes the listing as files finish, and asks before the tab
 * closes while files are queued, uploading or paused.
 */
export function useUploadQueue({ api, onUploaded }: UploadQueueOptions) {
  const [state, dispatch] = useReducer(queueReducer, EMPTY_QUEUE);
  const apiRef = useRef(api);
  apiRef.current = api;
  const controllers = useRef(new Map<string, AbortController>());
  const abortedSessions = useRef(new Set<string>());
  const nextId = useRef(0);

  // The listing's refresh, throttled: now, then once more after the burst.
  const uploadedRef = useRef(onUploaded);
  uploadedRef.current = onUploaded;
  const refresh = useRef<{ timer: ReturnType<typeof setTimeout> | null; dirty: boolean }>({ timer: null, dirty: false });
  const uploaded = useCallback(() => {
    const r = refresh.current;
    if (r.timer !== null) {
      r.dirty = true;
      return;
    }
    uploadedRef.current();
    const tick = () => {
      if (r.dirty) {
        r.dirty = false;
        uploadedRef.current();
        r.timer = setTimeout(tick, REFRESH_EVERY);
      } else r.timer = null;
    };
    r.timer = setTimeout(tick, REFRESH_EVERY);
  }, []);

  const launch = useCallback(
    (item: UploadItem) => {
      const controller = new AbortController();
      controllers.current.set(item.id, controller);
      const id = item.id;
      runUpload(
        apiRef.current,
        { bucket: item.bucket, key: item.key, name: item.name, file: item.file, choice: item.choice, session: item.session },
        {
          signal: controller.signal,
          onProgress: (loaded) => dispatch({ type: "progress", id, loaded, at: Date.now() }),
          onSession: (session) => dispatch({ type: "session", id, session }),
          onName: (name) => dispatch({ type: "name", id, name }),
        },
      ).then(
        ({ name }) => {
          if (controller.signal.aborted) return;
          dispatch({ type: "done", id, name });
          uploaded();
        },
        (err: unknown) => {
          // Paused or cancelled: the reducer already moved the file on.
          if (controller.signal.aborted) return;
          dispatch({ type: "fail", id, error: describeUploadError(err, item.file.type) });
        },
      ).finally(() => {
        if (controllers.current.get(id) === controller) controllers.current.delete(id);
      });
    },
    [uploaded],
  );

  useEffect(() => {
    for (const id of nextToStart(state)) dispatch({ type: "start", id, at: Date.now() });
    for (const item of state.items) {
      const running = controllers.current.get(item.id);
      if (item.status === "uploading" && !running) launch(item);
      else if (item.status !== "uploading" && running) {
        running.abort();
        controllers.current.delete(item.id);
      }
      if (item.status === "skipped" && item.cancelled && item.session !== null && !abortedSessions.current.has(item.session)) {
        abortedSessions.current.add(item.session);
        apiRef.current.abort(item.session).catch(() => {
          // Storage drops an abandoned session after 24 hours anyway.
        });
      }
    }
  }, [state, launch]);

  // Leaving the page (or this view) stops what's running.
  useEffect(() => {
    const running = controllers.current;
    const r = refresh.current;
    return () => {
      for (const c of running.values()) c.abort();
      running.clear();
      if (r.timer !== null) clearTimeout(r.timer);
      r.timer = null;
    };
  }, []);

  const active = hasActiveUploads(state);
  useEffect(() => {
    if (!active) return;
    const warn = (event: BeforeUnloadEvent) => {
      event.preventDefault();
      // Older browsers show the prompt only when returnValue is set.
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [active]);

  const add = useCallback((bucket: string, prefix: string, files: DroppedFile[]) => {
    const uploads = files.map((f) => ({ id: `u${++nextId.current}`, file: f.file, bucket, key: uploadKey(prefix, f.path) }));
    if (uploads.length > 0) dispatch({ type: "add", uploads });
  }, []);

  return {
    state,
    active,
    add,
    pause: useCallback((id: string) => dispatch({ type: "pause", id }), []),
    resume: useCallback((id: string) => dispatch({ type: "resume", id }), []),
    cancel: useCallback((id: string) => dispatch({ type: "cancel", id }), []),
    retry: useCallback((id: string) => dispatch({ type: "retry", id }), []),
    resolve: useCallback((id: string, choice: "skip" | ConflictChoice) => dispatch({ type: "resolve", id, choice }), []),
    clear: useCallback(() => dispatch({ type: "clear" }), []),
  };
}

export type UploadQueue = ReturnType<typeof useUploadQueue>;
