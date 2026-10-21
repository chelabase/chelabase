import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { CHANGED_TITLE, GONE_TEXT, NO_CHECK_NOTICE } from "@/lib/realtime/conflict";
import { ConflictDialog, type ConflictDialogProps } from "./conflict-dialog";

function dialog(over: Partial<ConflictDialogProps> = {}) {
  const props: ConflictDialogProps = {
    open: true,
    state: { kind: "changed", columns: [{ name: "amount_cents", theirs: "46000", yours: "47000" }, { name: "memo", theirs: null, yours: "mine" }] },
    unchecked: [],
    onOverwrite: vi.fn(),
    onTakeTheirs: vi.fn(),
    onClose: vi.fn(),
    ...over,
  };
  render(<ConflictDialog {...props} />);
  return { props, user: userEvent.setup() };
}

describe("ConflictDialog: the row changed", () => {
  it("is_titled_changed_by_someone_else", () => {
    dialog();
    expect(screen.getByRole("dialog", { name: CHANGED_TITLE })).toBeInTheDocument();
  });

  it("shows_theirs_and_yours_per_edited_column_with_null_as_NULL", () => {
    dialog();
    const d = screen.getByRole("dialog");
    const amount = within(d).getByRole("row", { name: /amount_cents/ });
    expect(amount).toHaveTextContent("46000");
    expect(amount).toHaveTextContent("47000");
    const memo = within(d).getByRole("row", { name: /memo/ });
    expect(memo).toHaveTextContent("NULL");
    expect(memo).toHaveTextContent("mine");
    expect(within(d).getByRole("columnheader", { name: "Theirs" })).toBeInTheDocument();
    expect(within(d).getByRole("columnheader", { name: "Yours" })).toBeInTheDocument();
  });

  it("overwrite_and_take_theirs_call_back", async () => {
    const { props, user } = dialog();
    await user.click(screen.getByRole("button", { name: "Overwrite" }));
    expect(props.onOverwrite).toHaveBeenCalledTimes(1);
    expect(props.onTakeTheirs).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Take theirs" }));
    expect(props.onTakeTheirs).toHaveBeenCalledTimes(1);
  });

  it("escape_closes_without_choosing", async () => {
    const { props, user } = dialog();
    await user.keyboard("{Escape}");
    expect(props.onClose).toHaveBeenCalled();
    expect(props.onOverwrite).not.toHaveBeenCalled();
    expect(props.onTakeTheirs).not.toHaveBeenCalled();
  });

  it("busy_disables_both_choices", () => {
    dialog({ busy: true });
    expect(screen.getByRole("button", { name: "Overwrite" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Take theirs" })).toBeDisabled();
  });

  it("an_error_is_shown_as_an_alert", () => {
    dialog({ error: "boom" });
    expect(screen.getByRole("alert")).toHaveTextContent("boom");
  });

  it("no_notice_when_every_edited_column_was_checked", () => {
    dialog();
    expect(screen.queryByText(new RegExp(NO_CHECK_NOTICE))).toBeNull();
  });

  it("unchecked_columns_get_the_no_conflict_check_notice", () => {
    dialog({ unchecked: ["meta"] });
    const notice = screen.getByText(new RegExp(NO_CHECK_NOTICE));
    expect(notice).toBeInTheDocument();
    expect(notice.parentElement).toHaveTextContent("meta");
  });

  it("renders_nothing_when_closed", () => {
    dialog({ open: false });
    expect(screen.queryByRole("dialog")).toBeNull();
  });
});

describe("ConflictDialog: the row is gone", () => {
  it("says_deleted_or_hidden_and_offers_no_overwrite", () => {
    dialog({ state: { kind: "gone" } });
    expect(screen.getByRole("dialog")).toHaveTextContent(GONE_TEXT);
    expect(screen.queryByRole("button", { name: "Overwrite" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Take theirs" })).toBeNull();
  });

  it("closes_with_a_close_button", async () => {
    const { props, user } = dialog({ state: { kind: "gone" } });
    await user.click(screen.getByRole("button", { name: "Close" }));
    expect(props.onClose).toHaveBeenCalled();
  });
});
