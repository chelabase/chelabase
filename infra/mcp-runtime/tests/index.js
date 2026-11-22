// `node --test infra/mcp-runtime/tests/` runs this directory's index: every test file.
require("./run.test.mjs");
require("./no-network.test.mjs");
require("./busy-loop.test.mjs");
require("./memory.test.mjs");
require("./hardening.test.mjs");
