import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  RAIL_BEHAVIOURS,
  RAIL_STORAGE_KEY,
  readRailBehaviour,
  useRailBehaviour,
  writeRailBehaviour,
} from "./rail-preference";

const denied = () => {
  throw new DOMException("denied", "SecurityError");
};

afterEach(() => {
  vi.restoreAllMocks();
  window.localStorage.clear();
});

describe("readRailBehaviour", () => {
  it("returns hover when nothing is stored", () => {
    expect(readRailBehaviour()).toBe("hover");
  });

  it("returns a stored valid value", () => {
    for (const value of RAIL_BEHAVIOURS) {
      window.localStorage.setItem(RAIL_STORAGE_KEY, value);
      expect(readRailBehaviour()).toBe(value);
    }
  });

  it("returns hover for an invalid stored value", () => {
    window.localStorage.setItem(RAIL_STORAGE_KEY, "wide");
    expect(readRailBehaviour()).toBe("hover");
  });

  it("returns hover when the storage getter throws", () => {
    expect(readRailBehaviour(denied)).toBe("hover");
  });

  it("returns hover when getItem throws", () => {
    const storage = { getItem: denied } as unknown as Storage;
    expect(readRailBehaviour(() => storage)).toBe("hover");
  });
});

describe("writeRailBehaviour", () => {
  it("write persists the value under chelabase.studio.rail", () => {
    writeRailBehaviour("collapsed");
    expect(window.localStorage.getItem("chelabase.studio.rail")).toBe("collapsed");
  });

  it("write swallows a throwing setItem and a throwing getter", () => {
    const storage = { setItem: denied } as unknown as Storage;
    expect(() => writeRailBehaviour("expanded", () => storage)).not.toThrow();
    expect(() => writeRailBehaviour("expanded", denied)).not.toThrow();
  });
});

describe("useRailBehaviour", () => {
  it("useRailBehaviour updates state even when storage is unavailable", () => {
    vi.spyOn(window, "localStorage", "get").mockImplementation(denied);
    const { result } = renderHook(() => useRailBehaviour());
    expect(result.current[0]).toBe("hover");
    act(() => result.current[1]("collapsed"));
    expect(result.current[0]).toBe("collapsed");
  });
});
