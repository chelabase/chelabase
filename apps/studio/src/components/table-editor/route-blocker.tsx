import { useBlocker } from "@tanstack/react-router";
import { useTableEditor } from "@/lib/table-editor/store";

/**
 * Blocks the router (Back, Forward, a link away from the table editor) while the row panel holds an unsaved draft,
 * with the editor's own "Discard changes to row N?" dialog: Keep editing cancels the navigation, Discard lets it go.
 * A navigation the editor already asked about (`guardDiscard`) finds no draft left and passes; closing the tab is
 * left to the editor's own `beforeunload` guard (one prompt per attempt).
 */
export function RouteDraftBlocker() {
  const askDiscard = useTableEditor((s) => s.askDiscard);
  const dirty = useTableEditor((s) => s.rowDraft !== null);
  useBlocker({
    shouldBlockFn: async () => !(await askDiscard()),
    enableBeforeUnload: false,
    disabled: !dirty,
  });
  return null;
}
