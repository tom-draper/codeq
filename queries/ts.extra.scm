; TypeScript-only definitions, appended to `JS_QUERY` for TS and TSX.

(interface_declaration name: (_) @name) @interface
(type_alias_declaration name: (_) @name) @type
(enum_declaration name: (_) @name) @enum
; Typed class fields, interface properties, and enum members.
(public_field_definition name: (property_identifier) @name) @field
(property_signature name: (property_identifier) @name) @field
(enum_assignment name: (property_identifier) @name) @variant
; A member with no initialiser is the bare identifier, not an `enum_assignment`,
; so matching only the assignment form indexed `enum E { X = 1 }` and left the
; commoner `enum E { A, B }` with no members at all.
(enum_body (property_identifier) @name @variant)
