import { ApiReferenceReact, type AnyApiReferenceConfiguration } from "@scalar/api-reference-react";
// Imported as a string and rendered in a <style> below, so it applies only while
// this page is mounted: Scalar's stylesheet has `body` and `:root` rules that
// must not outlive the page and restyle the rest of Studio.
import scalarCss from "@scalar/api-reference-react/style.css?inline";
import designScalarCss from "@chelabase/design/scalar.css?inline";
import { useEffect, useMemo, type CSSProperties } from "react";
import managementDocument from "../../../../openapi/management.json";
import { useAccessToken } from "@/lib/access-token";
import { useStudioEnv } from "@/lib/env-context";

/**
 * Self-hosted: the document is bundled (no fetch), and nothing calls scalar.com
 * (no telemetry, no default web fonts, no proxy). Omitting `proxyUrl` would fall
 * back to https://proxy.scalar.com on the web layout, so it is set to "" (no
 * proxy); the external URLs, agent, MCP and developer tools are switched off.
 */
export function apiReferenceConfiguration(apiUrl: string, token: string | undefined): AnyApiReferenceConfiguration {
  return {
    content: { ...managementDocument, servers: [{ url: apiUrl }] },
    telemetry: false,
    withDefaultFonts: false,
    proxyUrl: "",
    externalUrls: { dashboardUrl: "", registryUrl: "", proxyUrl: "", apiBaseUrl: "" },
    agent: { disabled: true },
    mcp: { disabled: true },
    showDeveloperTools: "never",
    defaultHttpClient: { targetKey: "js", clientKey: "axios" },
    forceDarkModeState: "dark",
    hideDarkModeToggle: true,
    authentication: {
      preferredSecurityScheme: "bearer",
      securitySchemes: { bearer: { token: token ?? "" } },
    },
  };
}

export default function ApiReference() {
  const { apiUrl } = useStudioEnv();
  const token = useAccessToken();
  // A new configuration only when the token changes: Scalar re-applies it, so a
  // refreshed token reaches "Try it out" without remounting.
  const configuration = useMemo(() => apiReferenceConfiguration(apiUrl, token), [apiUrl, token]);
  // Scalar toggles a color-mode class on document.body and never removes it.
  useEffect(
    () => () => {
      document.body.classList.remove("dark-mode", "light-mode");
    },
    [],
  );
  return (
    <>
      <style>{scalarCss}</style>
      {/* The design palette over Scalar's dark theme, scoped to .lagoon-scalar (@chelabase/design/scalar.css). */}
      <style>{designScalarCss}</style>
      {/* The shell's sticky header is 52px: Scalar's own sticky sidebar sits under it. */}
      <div
        className="lagoon-scalar"
        style={{ "--scalar-custom-header-height": "52px" } as CSSProperties}
      >
        <ApiReferenceReact configuration={configuration} />
      </div>
    </>
  );
}
