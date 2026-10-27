import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { DottedName } from "./merge-changes";

describe("DottedName", () => {
  it("breaks_after_each_dot_and_keeps_the_text", () => {
    const { container } = render(<span><DottedName name="public.orders.id" /></span>);
    expect(container.textContent).toBe("public.orders.id");
    expect(container.querySelectorAll("wbr")).toHaveLength(2);
    expect(container.innerHTML).toBe("<span>public.<wbr>orders.<wbr>id</span>");
  });
  it("a_name_without_dots_has_no_break_hint", () => {
    const { container } = render(<span><DottedName name="orders" /></span>);
    expect(container.querySelectorAll("wbr")).toHaveLength(0);
    expect(container.textContent).toBe("orders");
  });
});
