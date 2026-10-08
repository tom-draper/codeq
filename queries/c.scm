; C definitions and #include directives. Declarator nesting means some forms
; (e.g. pointer-returning functions) need their own pattern.

(function_definition declarator: (function_declarator declarator: (identifier) @name) body: (compound_statement) @body) @function
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name)) body: (compound_statement) @body) @function
(struct_specifier name: (type_identifier) @name body: (field_declaration_list) @body) @struct
(enum_specifier name: (type_identifier) @name body: (enumerator_list) @body) @enum
(preproc_include path: (system_lib_string) @name) @import
(preproc_include path: (string_literal) @name) @import
; Struct/union fields, including pointer declarators (`char *b`).
(field_declaration declarator: (field_identifier) @name) @field
(field_declaration declarator: (pointer_declarator declarator: (field_identifier) @name)) @field
(enumerator name: (identifier) @name) @variant
