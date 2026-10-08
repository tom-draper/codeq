; Go definitions and imports.

(function_declaration name: (identifier) @name body: (block) @body) @function
(method_declaration name: (field_identifier) @name body: (block) @body) @method
(type_declaration (type_spec name: (type_identifier) @name type: (struct_type)) @struct)
(type_declaration (type_spec name: (type_identifier) @name type: (interface_type)) @interface)
(type_declaration (type_spec name: (type_identifier) @name type: (type_identifier)) @type)
(type_declaration (type_alias name: (type_identifier) @name) @type)
(import_spec path: (interpreted_string_literal) @name) @import
; Struct fields. Embedded fields (no name) carry no identifier, so are skipped.
(field_declaration name: (field_identifier) @name) @field
