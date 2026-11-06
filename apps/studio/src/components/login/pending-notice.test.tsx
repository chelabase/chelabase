import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { PendingNotice } from "./pending-notice";

describe("PendingNotice", () => {
  it("says_an_admin_must_approve_and_points_back_to_sign_in", () => {
    render(<PendingNotice />);
    expect(screen.getByRole("heading", { name: "Waiting for approval" })).toBeInTheDocument();
    expect(screen.getByText(/needs to approve your account/)).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Back to sign in" })).toHaveAttribute("href", "/login");
  });
});
