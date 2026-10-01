import { Link } from "@tanstack/react-router";

export function ApiNavLink() {
  return (
    <Link to="/app/api" className="underline-offset-4 hover:underline">
      API
    </Link>
  );
}
