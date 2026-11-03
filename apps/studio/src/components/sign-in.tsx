import { Link } from "@tanstack/react-router";
import { buttonVariants } from "@/components/ui/button";

export interface SignInProps {
  /** Looking for a cookie session that is already there. */
  checking: boolean;
}

/** The landing page of a signed-out visitor: the way to the sign-in and account pages. */
export function SignIn({ checking }: SignInProps) {
  return (
    <main className="flex min-h-screen flex-col items-center justify-center gap-6 bg-background p-8 text-foreground">
      <h1 className="text-3xl font-semibold tracking-tight">Chelabase Studio</h1>
      <p className="max-w-md text-center text-muted-foreground">Postgres databases for your projects.</p>
      {checking ? (
        <p role="status" className="text-sm text-muted-foreground">Checking your session…</p>
      ) : (
        <div className="flex gap-3">
          <Link to="/login" search={{ notice: "" }} className={buttonVariants()}>
            Sign in
          </Link>
        </div>
      )}
    </main>
  );
}
