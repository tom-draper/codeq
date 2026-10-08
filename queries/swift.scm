(function_declaration name: (simple_identifier) @name body: (function_body) @body) @function
(function_declaration name: (simple_identifier) @name !body) @function
(protocol_function_declaration name: (simple_identifier) @name) @method
(class_declaration name: (type_identifier) @name body: (class_body) @body) @class
(class_declaration name: (type_identifier) @name !body) @class
(protocol_declaration name: (type_identifier) @name body: (protocol_body) @body) @interface
(protocol_declaration name: (type_identifier) @name !body) @interface
(import_declaration (identifier) @name) @import
; Stored properties (`var`/`let` in a type body).
(property_declaration (pattern (simple_identifier) @name)) @field
