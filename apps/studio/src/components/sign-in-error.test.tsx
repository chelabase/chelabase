import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { SignInError } from "./sign-in-error";

describe("SignInError", () => {
  it("explains the failure and links back to Studio", () => {
    render(<SignInError message="discovery failed" homeHref="/" />);
    expect(screen.getByRole("alert")).toHaveTextContent("Sign-in failed: discovery failed");
    expect(screen.getByRole("link", { name: "Back to Studio" })).toHaveAttribute("href", "/");
  });
});
