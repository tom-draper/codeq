; C#.

(class_declaration name: (identifier) @name body: (_) @body) @class
(interface_declaration name: (identifier) @name body: (_) @body) @interface
(struct_declaration name: (identifier) @name body: (_) @body) @struct
(enum_declaration name: (identifier) @name body: (_) @body) @enum
(record_declaration name: (identifier) @name) @record
(method_declaration name: (identifier) @name body: (block) @body) @method
(constructor_declaration name: (identifier) @name body: (block) @body) @method
(namespace_declaration name: (_) @name body: (_) @body) @namespace
(using_directive (identifier) @name) @import
(using_directive (qualified_name) @name) @import
; Fields, auto/expression properties, and enum members.
(field_declaration (variable_declaration (variable_declarator (identifier) @name))) @field
(property_declaration name: (identifier) @name) @field
(enum_member_declaration name: (identifier) @name) @variant
