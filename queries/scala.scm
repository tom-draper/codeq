(function_definition name: (identifier) @name body: (_) @body) @function
(function_definition name: (identifier) @name !body) @function
(function_declaration name: (identifier) @name) @function
(class_definition name: (identifier) @name body: (_) @body) @class
(class_definition name: (identifier) @name !body) @class
(object_definition name: (identifier) @name body: (_) @body) @class
(object_definition name: (identifier) @name !body) @class
(trait_definition name: (identifier) @name body: (_) @body) @trait
(trait_definition name: (identifier) @name !body) @trait
(enum_definition name: (identifier) @name) @enum
(type_definition name: (identifier) @name) @type
(import_declaration path: (_) @name) @import
; `val`/`var` members and case-class parameters (which are fields).
(val_definition pattern: (identifier) @name) @field
(var_definition pattern: (identifier) @name) @field
(class_parameter name: (identifier) @name) @field
(val_declaration name: (identifier) @name) @field
(var_declaration name: (identifier) @name) @field
(simple_enum_case name: (identifier) @name) @variant
