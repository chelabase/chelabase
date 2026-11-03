import ReactDOM from "react-dom/client";
import { RouterProvider, createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";
import { loadEnv, type StudioEnv } from "./lib/env";
import { EnvContext } from "./lib/env-context";
import { configureLoginClient } from "./lib/login-client/fetch";
import { parseSearch, stringifySearch } from "./lib/search";
import { createStudioSession } from "./lib/session";
import { SessionProvider } from "./lib/session-context";

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
  // The login client must know the API URL before the session mints its first token.
  configureLoginClient({ baseUrl: env.apiUrl });
  // One session per tab: the member token lives in this object's memory, nowhere else.
  const session = createStudioSession();
  root.render(
    <SessionProvider session={session}>
      <EnvContext.Provider value={env}>
        <RouterProvider router={router} />
      </EnvContext.Provider>
    </SessionProvider>,
  );
}

if (!rootElement.innerHTML) {
  const root = ReactDOM.createRoot(rootElement);
  // The runtime config (or, under the workbench's Vite server, the build values) before the
  // router mounts: the session and the login client depend on it.
  loadEnv(import.meta.env as Record<string, string | undefined>).then(
    (env) => mount(root, env),
    (err: unknown) => {
      root.render(<pre className="p-8 text-sm">{err instanceof Error ? err.message : String(err)}</pre>);
      console.error(err);
    },
  );
}
