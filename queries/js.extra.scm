; JavaScript-only definitions, appended to `JS_QUERY` for .js/.jsx.
;
; A class field is `field_definition` in the JavaScript grammar but
; `public_field_definition` in TypeScript's, so it cannot live in the shared
; query: a node type absent from a grammar fails the whole query to compile.

(field_definition property: (property_identifier) @name) @field
(field_definition property: (private_property_identifier) @name) @field
