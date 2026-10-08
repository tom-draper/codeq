; PHP (the `<?php` variant).

(function_definition name: (name) @name body: (compound_statement) @body) @function
(method_declaration name: (name) @name body: (compound_statement) @body) @method
(class_declaration name: (name) @name body: (declaration_list) @body) @class
(interface_declaration name: (name) @name body: (declaration_list) @body) @interface
(trait_declaration name: (name) @name body: (declaration_list) @body) @trait
(enum_declaration name: (name) @name) @enum
(namespace_use_clause (qualified_name) @name) @import
(property_declaration (property_element (variable_name (name) @name))) @field
(enum_case name: (name) @name) @variant
; File inclusion (require/include and their _once variants) with a static string
; path — PHP's other import mechanism. Both single-quoted (string) and
; double-quoted (encapsed_string) forms; quotes stripped like other imports.
(require_expression [(string) (encapsed_string)] @name) @import
(require_once_expression [(string) (encapsed_string)] @name) @import
(include_expression [(string) (encapsed_string)] @name) @import
(include_once_expression [(string) (encapsed_string)] @name) @import
; Typed and untyped properties, and enum cases.
(property_declaration (property_element (variable_name (name) @name))) @field
(enum_case name: (name) @name) @variant
