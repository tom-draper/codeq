(class_declaration name: (_) @name) @class
(object_declaration name: (_) @name) @class
(function_declaration name: (_) @name) @function
(import [(identifier) (qualified_identifier)] @name) @import
; `val`/`var` properties and primary-constructor parameters.
(property_declaration (variable_declaration (identifier) @name)) @field
(class_parameter (identifier) @name) @field
