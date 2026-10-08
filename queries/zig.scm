; Zig: functions, and the `const X = struct/enum/union {…}` type idiom.

; `@import("std")` — a builtin call, guarded on the builtin's name.
(builtin_function
  (builtin_identifier) @guard
  (arguments (string (string_content) @name))) @import

(function_declaration name: (identifier) @name body: (block) @body) @function
(variable_declaration (identifier) @name (struct_declaration)) @struct
(variable_declaration (identifier) @name (enum_declaration)) @enum
(variable_declaration (identifier) @name (union_declaration)) @union
(container_field name: (identifier) @name) @field
