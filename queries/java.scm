; Java definitions and imports.

(class_declaration name: (identifier) @name body: (class_body) @body) @class
(interface_declaration name: (identifier) @name body: (interface_body) @body) @interface
(enum_declaration name: (identifier) @name body: (enum_body) @body) @enum
(record_declaration name: (identifier) @name) @record
(method_declaration name: (identifier) @name body: (block) @body) @method
(method_declaration name: (identifier) @name !body) @method
(constructor_declaration name: (identifier) @name body: (constructor_body) @body) @method
(import_declaration (scoped_identifier) @name) @import
; Fields and enum constants.
(field_declaration declarator: (variable_declarator name: (identifier) @name)) @field
(enum_constant name: (identifier) @name) @variant
