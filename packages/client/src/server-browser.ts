// What `@chelabase/client/server` resolves to under the `browser` condition (and no server
// runtime's condition): a browser bundle that pulls it in fails instead of shipping server-only code.
throw new Error(
  "@chelabase/client/server runs only on the server: import @chelabase/client in browser code",
);

export {};
