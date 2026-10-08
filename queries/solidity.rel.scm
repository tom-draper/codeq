; Solidity: contracts/interfaces/libraries, functions, modifiers, structs, enums,
; events, and imports.

; `contract A is B, C` — Solidity has no separate implements form.
(contract_declaration name: (identifier) @sub
  (inheritance_specifier (user_defined_type (identifier) @extends)))
