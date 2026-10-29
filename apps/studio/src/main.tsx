import ReactDOM from "react-dom/client";
import { RouterProvider, createRouter } from "@tanstack/react-router";
import { AuthProvider } from "react-oidc-context";
import { routeTree } from "./routeTree.gen";
import { buildOidcConfig } from "./lib/auth";
import { loadEnv, type StudioEnv } from "./lib/env";
import { EnvContext } from "./lib/env-context";
import { parseSearch, stringifySearch } from "./lib/search";

const router = createRouter({
  routeTree,
  defaultPreload: "intent",
  scrollRestoration: true,
  parseSearch,
  stringifySearch,
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}

const rootElement = document.getElementById("app")!;

function mount(root: ReactDOM.Root, env: StudioEnv) {
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

if (!rootElement.innerHTML) {
  const root = ReactDOM.createRoot(rootElement);
  // The runtime config (or, under the workbench's Vite server, the build values) before the
  // router mounts: the OIDC settings depend on it.
  loadEnv(import.meta.env as Record<string, string | undefined>).then(
    (env) => mount(root, env),
    (err: unknown) => {
      root.render(<pre className="p-8 text-sm">{err instanceof Error ? err.message : String(err)}</pre>);
      console.error(err);
    },
  );
}
