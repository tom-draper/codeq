(contract_declaration name: (identifier) @name body: (contract_body) @body) @class
(interface_declaration name: (identifier) @name body: (contract_body) @body) @interface
(library_declaration name: (identifier) @name body: (contract_body) @body) @class
(function_definition name: (identifier) @name body: (_) @body) @function
(function_definition name: (identifier) @name !body) @function
(modifier_definition name: (identifier) @name) @function
(struct_declaration name: (identifier) @name) @struct
(enum_declaration name: (identifier) @name) @enum
(event_definition name: (identifier) @name) @type
(import_directive source: (string) @name) @import
; Contract state variables, and enum values.
(state_variable_declaration name: (identifier) @name) @field
; A Solidity `enum_value` is itself the identifier leaf, not a wrapper, so the
; name and the definition are the same node.
(constant_variable_declaration name: (identifier) @name) @field
(struct_member name: (identifier) @name) @field
(enum_value) @name @variant
