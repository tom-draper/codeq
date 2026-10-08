; Markup comments and literal attribute values. The `(attribute …)` scope is
; load-bearing and the one real divergence from Svelte: Vue puts JavaScript in
; *quoted* directive values (`@click="handleClick"`, `:prop="expr"`, `v-if="c"`),
; which the grammar models as a `directive_attribute` with its own
; `quoted_attribute_value`. Matching that bare would classify template
; expressions as strings, and `callers`/`callees` — which read code only —
; would go blind to every handler a template binds. Svelte needs no such scope
; because it spells expressions with braces (`on:click={handleClick}`), which
; are never quoted values. `{{ interpolation }}` holds code and stays
; unclassified; `<style>` is opaque, as in Svelte.

(comment) @comment
(attribute (quoted_attribute_value) @string)
