import type { Delivery, InviteDelivery } from "@chelabase/management-client";
import { RadioGroup } from "@/components/ui/radio-group";

const NO_EMAIL: Record<string, string> = {
  email_not_configured:
    "Email isn't set up on this instance, so invites go out as a link you send yourself. To send them by email, configure SMTP for the identity service.",
  identity_unavailable: "Couldn't check whether email is set up, so invites go out as a link for now.",
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

/** How the invitee gets the invite: email (when the instance can send it) or a one-time link. */
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
        <p className="max-w-[65ch] text-xs text-chalk-dim">{NO_EMAIL[info.reason ?? "email_not_configured"] ?? NO_EMAIL.email_not_configured}</p>
      )}
      {error && (
        <p role="alert" className="text-sm text-coral">
          {error}
        </p>
      )}
    </div>
  );
}
