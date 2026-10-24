import type { Delivery, InviteDelivery } from "@chelabase/management-client";
import { RadioGroup } from "@/components/ui/radio-group";

const NO_EMAIL: Record<string, string> = {
  no_email_provider:
    "Email isn't set up on this instance, so invites go out as a link you send yourself. To send them by email, add an SMTP provider in Zitadel.",
  zitadel_unavailable: "Couldn't check whether email is set up, so invites go out as a link for now.",
  zitadel_not_configured:
    "This instance has no identity-provider admin access configured, so invites go out as a link.",
};

export interface DeliveryFieldProps {
  /** `GET /v1/invites/delivery`; undefined while it loads. */
  info: InviteDelivery | undefined;
  value: Delivery;
  onChange: (d: Delivery) => void;
  /** Shown under the choice (the "send by email" answer). */
  error?: string;
  disabled?: boolean;
}

/** How the invitee gets the invite: email (when Zitadel can send it) or a one-time link. */
export function DeliveryField({ info, value, onChange, error, disabled }: DeliveryFieldProps) {
  if (info === undefined) return <p className="text-sm text-chalk-dim">Checking email…</p>;
  const options = [
    ...(info.email_available ? [{ value: "email" as const, label: "Email", hint: "We email them a link." }] : []),
    { value: "link" as const, label: "One-time link", hint: "You copy a link and send it yourself." },
  ];
  return (
    <div className="space-y-1.5">
      <RadioGroup inline label="How they get the invite" options={options} value={value} onValueChange={onChange} disabled={disabled} />
      {!info.email_available && (
        <p className="max-w-[65ch] text-xs text-chalk-dim">{NO_EMAIL[info.reason ?? "no_email_provider"] ?? NO_EMAIL.no_email_provider}</p>
      )}
      {error && (
        <p role="alert" className="text-sm text-coral">
          {error}
        </p>
      )}
    </div>
  );
}
