import { useState } from "react";
import { Check, Copy } from "lucide-react";
import { cn } from "@/lib/utils";
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

/** An icon-only copy button that sits against its field's edge (square, bordered like the field); "Copied." is said to assistive technology and shown as a check. */
export function CopyIconButton({ value, label, className }: { value: string; label: string; className?: string }) {
  const [copied, setCopied] = useState<"yes" | "failed" | null>(null);
  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
      setCopied("yes");
    } catch {
      setCopied("failed");
    }
  }
  return (
    <>
      <button
        type="button"
        aria-label={label}
        onClick={() => void copy()}
        className={cn(
          "inline-flex w-9 shrink-0 items-center justify-center border border-input bg-ink text-chalk-dim outline-none hover:text-chalk focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand",
          className,
        )}
      >
        {copied === "yes" ? <Check aria-hidden="true" size={14} strokeWidth={1.5} /> : <Copy aria-hidden="true" size={14} strokeWidth={1.5} />}
      </button>
      <span role="status" className="sr-only">
        {copied === "yes" ? "Copied." : copied === "failed" ? "Copy failed: select and copy it by hand." : ""}
      </span>
    </>
  );
}
