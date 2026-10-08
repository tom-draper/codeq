; Markup comments and literal attribute values. `<script>`/`<style>` contents
; are each a single opaque token here: script spans come from the nested JS/TS
; pass, while CSS in `<style>` stays unclassified (no CSS grammar), so a comment
; inside `<style>` reads as code to `search --in`.

(comment) @comment
(quoted_attribute_value) @string
