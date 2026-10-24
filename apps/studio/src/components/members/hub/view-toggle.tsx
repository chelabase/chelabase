import { LayoutGrid, List } from "lucide-react";
import { RadioGroup } from "@/components/ui/radio-group";

export type RosterView = "cards" | "list";

/** Cards or List, as a segmented radio group in the tab tone. */
export function ViewToggle({ value, onChange }: { value: RosterView; onChange: (next: RosterView) => void }) {
  return (
    <RadioGroup
      label="View"
      hideLabel
      layout="segmented"
      tone="tab"
      value={value}
      onValueChange={onChange}
      options={[
        { value: "cards", label: "Cards", icon: <LayoutGrid aria-hidden="true" size={16} strokeWidth={1.5} /> },
        { value: "list", label: "List", icon: <List aria-hidden="true" size={16} strokeWidth={1.5} /> },
      ]}
    />
  );
}
