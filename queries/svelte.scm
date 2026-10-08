; Svelte markup. Everything a `<script>` block declares comes from the nested
; JS/TS pass in `extract_svelte`; what the *markup* itself defines is a
; `{#snippet name(…)}` — Svelte 5's reusable fragment, invoked as
; `{@render name(…)}`, so the (textual) call graph links the two. The grammar
; exposes no body field, so a snippet's signature is its whole declaration
; capped, like Ruby's and Kotlin's.

(snippet_statement (snippet_start (snippet_name) @name)) @function
