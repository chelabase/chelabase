/** A failed sign-in, with a way out (a full load of Studio's landing page). */
export function SignInError({ message, homeHref = "/" }: { message: string; homeHref?: string }) {
  return (
    <main className="space-y-2 p-8">
      <p role="alert" className="text-destructive">
        Sign-in failed: {message}
      </p>
      <a href={homeHref} className="underline">
        Back to Studio
      </a>
    </main>
  );
}
