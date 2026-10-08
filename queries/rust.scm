(function_item name: (identifier) @name body: (block) @body) @function
; A trait method declaration has no body, so the pattern above misses it — yet it
; is the whole point of the trait, and what `implementors` sends you looking for.
(function_signature_item name: (identifier) @name) @method
(struct_item name: (type_identifier) @name) @struct
(enum_item name: (type_identifier) @name) @enum
(union_item name: (type_identifier) @name) @union
(trait_item name: (type_identifier) @name body: (declaration_list) @body) @trait
(mod_item name: (identifier) @name) @module
(const_item name: (identifier) @name) @constant
(static_item name: (identifier) @name) @constant
(type_item name: (type_identifier) @name) @typealias
(macro_definition name: (identifier) @name) @macro

; Members. `field` and `variant` carry an `owner` (the enclosing type), because
; member names collide constantly across types and mean little alone.
(field_declaration name: (field_identifier) @name) @field
(enum_variant name: (identifier) @name) @variant

; Raw syntactic imports: name is the module path as written in source. A direct
; `use crate::util::make as m` also records `m` as the local binding, so call and
; context expansion can connect `m()` to `make` in util rather than treating it
; as an unrelated name. Braced use lists keep the whole declaration as one raw
; import for now: their per-item prefixes need a separate path reconstruction.
(use_declaration argument: [
  (identifier)
  (scoped_identifier)
  (scoped_use_list)
  (use_list)
  (use_wildcard)
  (crate)
  (self)
  (super)
] @name) @import
(use_declaration argument: (use_as_clause path: (_) @name alias: (identifier) @binding)) @import
; An external `mod foo;` (no inline body) pulls in another file's module — a
; file-level dependency edge, so treat it as an import for reachability. Inline
; `mod foo { … }` (has a body) is not a separate file, so exclude it via `!body`.
(mod_item name: (identifier) @name !body) @import
; Members: struct/union fields and enum variants. Contained in their parent's
; byte range, which is how `members` and `outline` recover the relationship.
(field_declaration name: (field_identifier) @name) @field
(enum_variant name: (identifier) @name) @variant
