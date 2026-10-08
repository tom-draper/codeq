(class_declaration
  name: (type_identifier) @sub
  (class_heritage (extends_clause value: (identifier) @extends)))
; A namespaced base class — `extends React.Component`, the shape every React
; class component is written in. The value is a `member_expression`, not an
; identifier, so the pattern above sees nothing and the class came back with no
; supertypes at all. The supertype is stored dotted, as written; `implementors`
; also matches on the last segment so `implementors Component` still finds it.
(class_declaration
  name: (type_identifier) @sub
  (class_heritage (extends_clause value: (member_expression) @extends)))
(class_declaration
  name: (type_identifier) @sub
  (class_heritage (implements_clause (type_identifier) @implements)))
; `implements ns.Iface` and `implements Iface<T>`.
(class_declaration
  name: (type_identifier) @sub
  (class_heritage (implements_clause (nested_type_identifier) @implements)))
(class_declaration
  name: (type_identifier) @sub
  (class_heritage (implements_clause (generic_type (type_identifier) @implements))))
(class_declaration
  name: (type_identifier) @sub
  (class_heritage (implements_clause (generic_type (nested_type_identifier) @implements))))
(interface_declaration
  name: (type_identifier) @sub
  (extends_type_clause (type_identifier) @extends))
(interface_declaration
  name: (type_identifier) @sub
  (extends_type_clause (nested_type_identifier) @extends))
(interface_declaration
  name: (type_identifier) @sub
  (extends_type_clause (generic_type (type_identifier) @extends)))
