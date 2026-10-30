// What `@chelabase/client/server` resolves to under the `browser` and `worker` conditions: a
// bundle for the browser that pulls it in fails instead of shipping server-only code.
throw new Error(
  "@chelabase/client/server runs only on the server: import @chelabase/client in browser code",
);

export {};
