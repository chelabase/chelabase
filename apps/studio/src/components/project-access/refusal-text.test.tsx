import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { RefusalText } from "./refusal-text";

describe("RefusalText", () => {
  it("draws_backticked_names_as_mono_spans_without_the_backticks", () => {
    const { container } = render(<p><RefusalText text="`app_login` is reserved on `ledger`." /></p>);
    expect(container).toHaveTextContent("app_login is reserved on ledger.");
    expect(container.textContent).not.toContain("`");
    expect(screen.getByText("app_login")).toHaveClass("font-mono");
  });

  it("leaves_an_unpaired_backtick_as_text", () => {
    const { container } = render(<p><RefusalText text="odd ` one" /></p>);
    expect(container).toHaveTextContent("odd ` one");
  });
});
