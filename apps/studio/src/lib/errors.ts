import { ApiError } from "@chelabase/management-client";

export function errorText(err: unknown): string {
  if (err instanceof ApiError) {
    return err.code === "network" ? "Can't reach Chelabase; check your connection" : err.message;
  }
  if (err instanceof Error) return err.message;
  return "Something went wrong";
}
