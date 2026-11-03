import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { LoginProviders } from "@/components/login/login-providers";
import { SignUpForm } from "@/components/login/sign-up-form";

export const Route = createFileRoute("/register")({ component: RegisterPage });

function RegisterPage() {
  const navigate = useNavigate();
  return (
    <LoginProviders>
      <SignUpForm
        toApp={() => void navigate({ to: "/app", replace: true })}
        toVerify={() => void navigate({ to: "/verify", search: { code: "" } })}
        toPending={() => void navigate({ to: "/pending", replace: true })}
        toLogin={(notice) => void navigate({ to: "/login", search: { notice }, replace: true })} />
    </LoginProviders>
  );
}
