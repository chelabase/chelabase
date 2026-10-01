import { useState } from "react";
import { Button } from "@/components/ui/button";

/** Copies `value` to the clipboard; says "Copied." or why it couldn't beside the button. */
export function CopyButton({ value, label, text = "Copy" }: { value: string; label: string; text?: string }) {
  const [status, setStatus] = useState<string | null>(null);
  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
      setStatus("Copied.");
    } catch {
      setStatus("Copy failed — select and copy manually.");
    }
  }
  return (
    <span className="inline-flex items-center gap-2">
      <Button size="xs" variant="ghost" aria-label={label} onClick={() => void copy()}>
        {text}
      </Button>
      {status && (
        <span role="status" className="text-xs text-muted-foreground">
          {status}
        </span>
      )}
    </span>
  );
}
