import ReactDOM from "react-dom/client";
import { RouterProvider, createRouter } from "@tanstack/react-router";
import { AuthProvider } from "react-oidc-context";
import { routeTree } from "./routeTree.gen";
import { buildOidcConfig } from "./lib/auth";
import { readEnv, type StudioEnv } from "./lib/env";
import { EnvContext } from "./lib/env-context";
import { canonicalRedirect } from "./lib/origin";

const router = createRouter({
  routeTree,
  defaultPreload: "intent",
  scrollRestoration: true,
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}

const rootElement = document.getElementById("app")!;
const redirect = canonicalRedirect(new URL(window.location.href));

if (redirect) {
  window.location.replace(redirect);
} else if (!rootElement.innerHTML) {
  const root = ReactDOM.createRoot(rootElement);
  let env: StudioEnv;
  try {
    env = readEnv(import.meta.env as Record<string, string | undefined>);
  } catch (err) {
    root.render(<pre className="p-8 text-sm">{err instanceof Error ? err.message : String(err)}</pre>);
    throw err;
  }
  root.render(
    <AuthProvider
      {...buildOidcConfig(env, window.location.origin)}
      // Leave /callback (and its code/state query) once tokens are stored.
      onSigninCallback={() => void router.navigate({ to: "/app", replace: true })}
    >
      <EnvContext.Provider value={env}>
        <RouterProvider router={router} />
      </EnvContext.Provider>
    </AuthProvider>,
  );
}
