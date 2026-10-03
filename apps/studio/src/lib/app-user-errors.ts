import { ApiError } from "@cheladb/management-client";
import { errorText } from "@/lib/errors";

/** Plain texts for the error codes app-user screens can meet. */
const TEXTS: Record<string, string> = {
  app_users_has_users: "Delete the project's users first",
  app_users_table_exists: "That table already exists. Pick another table name, or another schema.",
  data_api_not_enabled: "Enable the data API on every branch of this database first",
  app_users_location_fixed: "The database, schema and table can't change once app users are enabled. Disable them first.",
  email_taken: "That email is already in use",
  weak_password: "That password is too weak. Use a longer one.",
  unavailable: "User administration isn't configured on this Cheladb (the Zitadel admin credentials are missing)",
};

export function appUserErrorText(err: unknown): string {
  return err instanceof ApiError && TEXTS[err.code] ? TEXTS[err.code] : errorText(err);
}
