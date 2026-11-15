// `node --test infra/functions-runtime/tests/` runs this directory's index: every test file.
require("./runtime.test.mjs");
require("./busy-loop.test.mjs");
require("./start-failure.test.mjs");
require("./binding.test.mjs");
require("./scheduled.test.mjs");
require("./response-headers.test.mjs");
require("./storage.test.mjs");
