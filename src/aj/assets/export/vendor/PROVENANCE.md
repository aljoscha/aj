# Vendored libraries

Third-party assets bundled verbatim into the HTML export so the file is
self-contained and renders offline. Refresh by re-fetching the exact
pinned URLs below, then re-run the export smoke test
(`node ../smoke_test.mjs`).

- `marked.min.js` — marked v15.0.4 (MIT), full text in `marked.LICENSE`
  https://cdn.jsdelivr.net/npm/marked@15.0.4/marked.min.js
- `highlight.min.js` — highlight.js v11.11.1, common languages build
  (BSD-3-Clause), full text in `highlight.LICENSE`
  https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.11.1/highlight.min.js

The license texts are embedded into every exported file (in an HTML
comment) so each shared copy carries the required notices.
