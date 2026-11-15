import { describe, expect, it } from "vitest";
import { deploymentTarget, serviceSpecOf, serviceStatus } from "./functions-services";

const fn = (over: Record<string, unknown> = {}) =>
  ({ kind: "service", enabled: true, deployment_id: "dep1", health: null, last_error: null, ...over }) as never;

describe("serviceSpecOf", () => {
  it("reads_a_container_spec", () => {
    expect(serviceSpecOf({ backend: "container", image: "ghcr.io/acme/echo@sha256:" + "a".repeat(64), port: 8080, health_path: "/healthz" })).toEqual({
      backend: "container",
      image: "ghcr.io/acme/echo@sha256:" + "a".repeat(64),
      url: null,
      port: 8080,
      health_path: "/healthz",
    });
  });

  it("reads_an_external_spec", () => {
    expect(serviceSpecOf({ backend: "external", url: "https://api.example.com/v1", health_path: null })).toEqual({
      backend: "external",
      image: null,
      url: "https://api.example.com/v1",
      port: null,
      health_path: null,
    });
  });

  it("is_null_for_a_function_or_anything_unreadable", () => {
    expect(serviceSpecOf(null)).toBeNull();
    expect(serviceSpecOf({})).toBeNull();
    expect(serviceSpecOf({ backend: 7 })).toBeNull();
    expect(serviceSpecOf("container")).toBeNull();
    // Wrong-typed fields are dropped, not trusted.
    expect(serviceSpecOf({ backend: "container", image: 5, port: "80" })).toEqual({ backend: "container", image: null, url: null, port: null, health_path: null });
  });
});

describe("deploymentTarget", () => {
  it("names_the_image_or_url", () => {
    expect(deploymentTarget({ spec: { backend: "container", image: "echo@sha256:abc", port: 80 } } as never)).toBe("echo@sha256:abc");
    expect(deploymentTarget({ spec: { backend: "external", url: "https://x.test" } } as never)).toBe("https://x.test");
    expect(deploymentTarget({ spec: null } as never)).toBeNull();
  });
});

describe("serviceStatus", () => {
  it("is_null_for_a_function", () => {
    expect(serviceStatus(fn({ kind: "function" }))).toBeNull();
  });

  it("follows_enabled_deployment_and_health", () => {
    expect(serviceStatus(fn({ enabled: false }))).toEqual({ label: "Stopped", tone: "dim" });
    expect(serviceStatus(fn({ deployment_id: null }))).toEqual({ label: "Not deployed", tone: "dim" });
    expect(serviceStatus(fn({ health: null }))).toEqual({ label: "Starting", tone: "dim" });
    expect(serviceStatus(fn({ health: "healthy" }))).toEqual({ label: "Running", tone: "good" });
    expect(serviceStatus(fn({ health: "unhealthy" }))).toEqual({ label: "Unhealthy", tone: "bad" });
  });

  it("a_pending_switch_wins_over_the_server_state", () => {
    // Turned on in the list, the server still says off: show it starting.
    expect(serviceStatus(fn({ enabled: false, health: null }), true)).toEqual({ label: "Starting", tone: "dim" });
    expect(serviceStatus(fn({ enabled: true, health: "healthy" }), false)).toEqual({ label: "Stopped", tone: "dim" });
  });
});
