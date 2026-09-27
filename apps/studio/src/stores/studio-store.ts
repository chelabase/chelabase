import { create } from "zustand";

interface StudioState {
  ready: boolean;
  setReady: (ready: boolean) => void;
}

/** Placeholder Zustand store — no feature state yet. */
export const useStudioStore = create<StudioState>((set) => ({
  ready: true,
  setReady: (ready) => set({ ready }),
}));
