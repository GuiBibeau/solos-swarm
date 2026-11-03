# Lightpanda 1.0.2 probe: what its outputs show after a fill

Binary: lightpanda-x86_64-linux 1.0.2, sha256 ae7d01d261efb9e480e18ce3424533332883863bcf70b12a4321b5733f37e19e,
run on remote-ai (glibc 2.41) with LIGHTPANDA_DISABLE_TELEMETRY=true, `serve --host 127.0.0.1`.
Page: site/login.html (a same-origin login form, a second form posting to another origin, and a script that
echoes the password field into a div on input). Script: probe.mjs / probe2.mjs (flattened Target sessions).

1. Password inputs are masked by Lightpanda itself: `value: "*****"` in LP.getInteractiveElements,
   LP.detectForms and LP.getNodeDetails, before and after a fill. Text inputs show their real value.
2. DOM.getOuterHTML does not serialize an input's typed value (no value attribute is written).
3. LP.fillNode {backendNodeId, text} fills and fires the page's `input` listener (result `[]`).
4. A page that echoes the value leaks it through page text: the echo div shows up in
   DOM.getOuterHTML (raw), LP.getSemanticTree (a text node's `nodeValue`, raw) and LP.getMarkdown
   **markdown-escaped** (`pw\-Probe\-7f3a9c`): an exact-match scrubber misses the markdown form.
   The scrubber must also match the markdown-escaped encoding (backslash before ASCII punctuation),
   or normalize markdown before matching.
5. LP.detectForms reports each form's `action` (as written, e.g. "/session" or an absolute URL) and its
   fields with backendNodeIds, so the form-action check can resolve `action` against the page URL.
6. Fetch.enable {patterns:[{urlPattern:"*"}]} pauses the top navigation; Fetch.continueRequest with a
   headers array replacing the request headers delivered `Authorization: Bearer ...` to the server.
7. Event kinds seen: Target.targetCreated, Target.attachedToTarget, Page.frameStartedNavigating,
   Page.frameStartedLoading, Runtime.executionContextsCleared, Page.frameNavigated, DOM.documentUpdated,
   Page.domContentEventFired, Page.loadEventFired, Page.frameStoppedLoading, DOM.setChildNodes,
   Fetch.requestPaused.
