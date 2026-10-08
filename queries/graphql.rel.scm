; `type User implements Node & Timestamped`. The grammar nests
; `implements_interfaces` left-recursively rather than listing siblings, so
; `A & B & C` is `II(II(II(A), B), C)` and a single pattern binds only the
; outermost. One pattern per nesting level therefore, which covers up to five
; interfaces on a type — a stated bound rather than a silent truncation, and
; past anything real schemas do.

(object_type_definition
  (name) @sub
  (implements_interfaces (named_type (name) @implements)))
(interface_type_definition
  (name) @sub
  (implements_interfaces (named_type (name) @implements)))
(object_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (named_type (name) @implements))))
(interface_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (named_type (name) @implements))))
(object_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (implements_interfaces (named_type (name) @implements)))))
(interface_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (implements_interfaces (named_type (name) @implements)))))
(object_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (implements_interfaces (implements_interfaces (named_type (name) @implements))))))
(interface_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (implements_interfaces (implements_interfaces (named_type (name) @implements))))))
(object_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (implements_interfaces (implements_interfaces (implements_interfaces (named_type (name) @implements)))))))
(interface_type_definition
  (name) @sub
  (implements_interfaces (implements_interfaces (implements_interfaces (implements_interfaces (implements_interfaces (named_type (name) @implements)))))))
