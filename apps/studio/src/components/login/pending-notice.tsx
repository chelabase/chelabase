import { LoginLayout } from "./sign-in-form";

/**
 * Where a sign-in or sign-up lands when an admin hasn't approved the person yet. There is no
 * session yet (auth sets no cookie for `pending`), so this page can't check on its own: they sign
 * in again once they're approved.
 */
export function PendingNotice() {
  return (
    <LoginLayout title="Waiting for approval">
      <p className="text-sm">
        A tenant admin needs to approve your account before you can use Chelabase Studio.
      </p>
      <p className="text-sm text-muted-foreground">Once you're approved, sign in again to open Studio.</p>
      <a href="/login" className="text-sm underline">Back to sign in</a>
    </LoginLayout>
  );
}
