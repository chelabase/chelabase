import { loadConfig } from "./config.ts";
import { startServer } from "./server.ts";

const server = await startServer(loadConfig());
console.log(`identity: listening on ${server.url}`);

for (const signal of ["SIGINT", "SIGTERM"] as const) {
  process.once(signal, () => {
    void server.close().then(() => process.exit(0));
  });
}
