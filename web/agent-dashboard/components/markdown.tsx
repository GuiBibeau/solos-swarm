// What a model or a person wrote, as markdown, rendered safely. react-markdown builds React elements from the
// markdown tree and has no HTML in it unless `rehype-raw` is added, which it is not: a `<script>` or an `<img onerror>`
// in a message is dropped (`skipHtml`), never rendered. Three more rules, because the text can be steered by whatever
// the agent read on the web:
//   - a link's address passes react-markdown's default transform (http, https, mailto and the like; never
//     `javascript:` or `data:`) and opens in a new tab with no referrer;
//   - an image is never loaded: opening this page must not make the browser fetch an address a model chose, which
//     could carry text out in its query. It is shown as a link;
//   - tables and code scroll inside their box, so a wide one cannot make the page wider than a phone.

import type { ComponentPropsWithoutRef } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

function Link({ href, children }: ComponentPropsWithoutRef<"a">) {
  if (!href) return <span>{children}</span>;
  return (
    <a href={href} target="_blank" rel="noopener noreferrer nofollow">
      {children}
    </a>
  );
}

function ImageAsLink({ src, alt }: ComponentPropsWithoutRef<"img">) {
  if (typeof src !== "string" || !/^https?:\/\//i.test(src)) {
    return <span className="md-image">[image{alt ? `: ${alt}` : ""}]</span>;
  }
  return (
    <a className="md-image" href={src} target="_blank" rel="noopener noreferrer nofollow">
      [image{alt ? `: ${alt}` : ""}]
    </a>
  );
}

// react-markdown hands every component the syntax node it came from; it is not an attribute.
function Table({ node, ...props }: ComponentPropsWithoutRef<"table"> & { node?: unknown }) {
  void node;
  return (
    <div className="md-scroll">
      <table {...props} />
    </div>
  );
}

export function Markdown({ children }: { children: string }) {
  return (
    <div className="md">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        skipHtml
        components={{ a: Link, img: ImageAsLink, table: Table }}
      >
        {children}
      </ReactMarkdown>
    </div>
  );
}
