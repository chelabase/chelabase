// Outgoing mail over SMTP (nodemailer), configured from `CHELABASE_SMTP_*`. The one place
// identity sends email. Message bodies hold codes, so nothing here logs a message or an SMTP
// reply text, only error codes.
import nodemailer, { type Transporter } from "nodemailer";
import type { SmtpConfig } from "../config.ts";
import { HttpError } from "../http.ts";
import type { RenderedEmail } from "./templates.ts";

export interface EmailStatus {
  configured: boolean;
  reachable: boolean;
}

export interface Mailer {
  readonly configured: boolean;
  /** Whether SMTP is configured and answers (connect, EHLO, TLS and auth as configured). */
  status(): Promise<EmailStatus>;
  send(to: string, email: RenderedEmail): Promise<void>;
}

const TIMEOUT_MS = 10_000;

/** A short, code-free description of a nodemailer error for the log. */
function describe(err: unknown): string {
  if (err && typeof err === "object") {
    const e = err as { code?: unknown; responseCode?: unknown; command?: unknown };
    return [e.code, e.responseCode, e.command].filter((v) => typeof v === "string" || typeof v === "number").join(" ") || "unknown error";
  }
  return "unknown error";
}

class NoMailer implements Mailer {
  readonly configured = false;
  async status(): Promise<EmailStatus> {
    return { configured: false, reachable: false };
  }
  async send(): Promise<void> {
    throw new HttpError(409, "email_not_configured", "no SMTP server is configured");
  }
}

class SmtpMailer implements Mailer {
  readonly configured = true;
  private readonly transport: Transporter;
  private readonly from: string;

  constructor(smtp: SmtpConfig) {
    this.from = smtp.from;
    this.transport = nodemailer.createTransport({
      host: smtp.host,
      port: smtp.port,
      secure: smtp.tls === "tls",
      requireTLS: smtp.tls === "starttls",
      ignoreTLS: smtp.tls === "none",
      ...(smtp.user === undefined ? {} : { auth: { user: smtp.user, pass: smtp.password! } }),
      connectionTimeout: TIMEOUT_MS,
      greetingTimeout: TIMEOUT_MS,
      socketTimeout: TIMEOUT_MS,
      logger: false,
      debug: false,
    });
  }

  async status(): Promise<EmailStatus> {
    try {
      await this.transport.verify();
      return { configured: true, reachable: true };
    } catch (err) {
      console.error(`identity: SMTP check failed: ${describe(err)}`);
      return { configured: true, reachable: false };
    }
  }

  async send(to: string, email: RenderedEmail): Promise<void> {
    try {
      await this.transport.sendMail({ from: this.from, to, subject: email.subject, text: email.text, html: email.html });
    } catch (err) {
      console.error(`identity: SMTP send failed: ${describe(err)}`);
      throw new HttpError(502, "email_send_failed", "the SMTP server did not accept the message");
    }
  }
}

export function createMailer(smtp: SmtpConfig | undefined): Mailer {
  return smtp === undefined ? new NoMailer() : new SmtpMailer(smtp);
}
