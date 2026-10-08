; C++ (a superset of C): add classes, namespaces, and member functions.

(function_definition declarator: (function_declarator declarator: (identifier) @name) body: (compound_statement) @body) @function
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name)) body: (compound_statement) @body) @function
(function_definition declarator: (function_declarator declarator: (field_identifier) @name) body: (compound_statement) @body) @method
(function_definition declarator: (function_declarator declarator: (qualified_identifier) @name) body: (compound_statement) @body) @method
; Declaration-only members (the norm in headers): method/constructor prototypes
; with no body. Without these, a header-only class exposes none of its methods.
(field_declaration declarator: (function_declarator declarator: (field_identifier) @name)) @method
(field_declaration declarator: (function_declarator declarator: (identifier) @name)) @method
; Free-function prototypes at file scope (also headers).
(declaration declarator: (function_declarator declarator: (identifier) @name)) @function
(class_specifier name: (type_identifier) @name body: (field_declaration_list) @body) @class
(struct_specifier name: (type_identifier) @name body: (field_declaration_list) @body) @struct
(enum_specifier name: (type_identifier) @name body: (enumerator_list) @body) @enum
(namespace_definition name: (namespace_identifier) @name body: (declaration_list) @body) @namespace
(preproc_include path: (system_lib_string) @name) @import
(preproc_include path: (string_literal) @name) @import
(field_declaration declarator: (field_identifier) @name) @field
(field_declaration declarator: (pointer_declarator declarator: (field_identifier) @name)) @field
(enumerator name: (identifier) @name) @variant
