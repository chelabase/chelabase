// Email templates: the project name a caller passes is markup-escaped in every HTML part. No
// database needed.
import { expect, it } from "vitest";
import { render, TEMPLATE_KINDS } from "../src/email/templates.ts";

it("project name is escaped", () => {
  const project = `Acme <b onclick="x">&'`;
  const escaped = "Acme &lt;b onclick=&quot;x&quot;&gt;&amp;&#39;";
  for (const kind of TEMPLATE_KINDS) {
    const mail = render(kind, { project, code: "123456", expires: "15 minutes", link: "https://acme.test/" });
    expect(mail.html, kind).toContain(escaped);
    expect(mail.html, kind).not.toContain("<b ");
    // The plain parts carry the name as given.
    expect(mail.text, kind).toContain(project);
    expect(mail.subject, kind).toContain(project);
  }
});
