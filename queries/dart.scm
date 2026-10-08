(function_declaration signature: (function_signature name: (identifier) @name) body: (function_body) @body) @function
(method_declaration (method_signature (function_signature name: (identifier) @name)) body: (function_body) @body) @method
(class_declaration name: (identifier) @name body: (class_body) @body) @class
(mixin_declaration name: (identifier) @name body: (class_body) @body) @class
(enum_declaration name: (identifier) @name body: (_) @body) @enum
(library_import (import_specification uri: (configurable_uri (uri (string_literal) @name)))) @import
(class_member (declaration (initialized_identifier_list
  (initialized_identifier name: (identifier) @name)))) @field
(class_member (declaration (static_final_declaration_list
  (static_final_declaration name: (identifier) @name)))) @field
(enum_constant name: (identifier) @name) @variant
