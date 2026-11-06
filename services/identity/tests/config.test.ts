// Secret files: the same rules as the Rust services' `FileValue` secrets (owner-only, regular
// file, no symlink). No database needed.
import { randomBytes } from "node:crypto";
import { chmodSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { loadConfig, smtpFromEnv } from "../src/config.ts";

let dir: string;
beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "identity-config-"));
});
afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

const SECRET_VALUE = `s3cret-${randomBytes(16).toString("hex")}`;

function file(name: string, value: string, mode = 0o600): string {
  const path = join(dir, name);
  writeFileSync(path, `${value}\n`);
  chmodSync(path, mode);
  return path;
}

/** A complete, valid environment whose files are all 0600. */
function baseEnv(): NodeJS.ProcessEnv {
  return {
    CHELABASE_IDENTITY_DATABASE_URL_FILE: file("database.url", "postgres://u:pw@127.0.0.1:1/db"),
    CHELABASE_IDENTITY_SECRET_FILE: file("secret", randomBytes(32).toString("hex")),
    CHELABASE_IDENTITY_AUTH_TOKEN_FILE: file("auth.token", randomBytes(32).toString("hex")),
    CHELABASE_IDENTITY_BOOTSTRAP_TOKEN_FILE: file("bootstrap.token", randomBytes(32).toString("hex")),
  };
}

function smtpEnv(passwordPath: string): NodeJS.ProcessEnv {
  return {
    CHELABASE_SMTP_HOST: "smtp.example.com",
    CHELABASE_SMTP_FROM: "chelabase@example.com",
    CHELABASE_SMTP_USER: "chelabase",
    CHELABASE_SMTP_PASSWORD_FILE: passwordPath,
  };
}

/** The error's message, which must never carry the value. */
function failure(f: () => unknown): string {
  try {
    f();
  } catch (e) {
    const message = (e as Error).message;
    expect(message).not.toContain(SECRET_VALUE);
    return message;
  }
  throw new Error("expected a failure");
}

describe("SMTP password file", () => {
  it("accepts_an_owner_only_file", () => {
    const smtp = smtpFromEnv(smtpEnv(file("smtp.password", SECRET_VALUE)));
    expect(smtp?.password).toBe(SECRET_VALUE);
  });

  it.each(["0640", "0644", "0604", "0660"])("refuses_mode_%s", (mode) => {
    const path = file("smtp.password", SECRET_VALUE, parseInt(mode, 8));
    const message = failure(() => smtpFromEnv(smtpEnv(path)));
    expect(message).toContain("CHELABASE_SMTP_PASSWORD_FILE");
    expect(message).toMatch(/chmod 600/);
  });

  it("refuses_a_symlink", () => {
    const target = file("real.password", SECRET_VALUE);
    const link = join(dir, "smtp.password");
    symlinkSync(target, link);
    expect(failure(() => smtpFromEnv(smtpEnv(link)))).toMatch(/symlink/);
  });

  it("refuses_anything_but_a_regular_file", () => {
    // Compose binds /dev/null when CHELABASE_SMTP_PASSWORD_PATH is unset.
    expect(failure(() => smtpFromEnv(smtpEnv("/dev/null")))).toMatch(/not a regular file/);
  });
});

describe("the service's own secret files", () => {
  it("loads_owner_only_files", () => {
    const config = loadConfig(baseEnv());
    expect(config.callerTokens.size).toBe(2);
  });

  it.each([
    "CHELABASE_IDENTITY_DATABASE_URL_FILE",
    "CHELABASE_IDENTITY_SECRET_FILE",
    "CHELABASE_IDENTITY_AUTH_TOKEN_FILE",
    "CHELABASE_IDENTITY_BOOTSTRAP_TOKEN_FILE",
  ])("refuses_%s_readable_by_group_or_others", (name) => {
    const env = baseEnv();
    for (const mode of [0o640, 0o644]) {
      chmodSync(env[name]!, mode);
      const message = failure(() => loadConfig(env));
      expect(message).toContain(name);
      expect(message).toMatch(/chmod 600/);
    }
  });

  it("refuses_a_group_writable_file", () => {
    const env = baseEnv();
    chmodSync(env.CHELABASE_IDENTITY_SECRET_FILE!, 0o620);
    expect(failure(() => loadConfig(env))).toMatch(/CHELABASE_IDENTITY_SECRET_FILE.*(writable|chmod)/);
  });
});
