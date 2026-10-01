import { ApiError } from "@cheladb/management-client";

export function errorText(err: unknown): string {
  if (err instanceof ApiError) {
    return err.code === "network" ? "Can't reach Cheladb; check your connection" : err.message;
  }
  if (err instanceof Error) return err.message;
  return "Something went wrong";
}
