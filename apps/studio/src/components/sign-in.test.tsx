import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { renderWithRouter } from "@/test/render-router";
import { SignIn } from "./sign-in";

describe("SignIn", () => {
  it("offers sign-in once the session check is over", async () => {
    renderWithRouter(<SignIn checking={false} />);
    expect(await screen.findByRole("link", { name: "Sign in" })).toHaveAttribute("href", expect.stringMatching(/^\/login/));
  });

  it("says_it_is_checking_and_offers_nothing_yet", async () => {
    renderWithRouter(<SignIn checking />);
    expect(await screen.findByRole("status")).toHaveTextContent("Checking your session");
    expect(screen.queryByRole("link", { name: "Sign in" })).toBeNull();
  });
});
