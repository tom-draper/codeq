; SQL. A schema is the one part of a polyglot repo that every language talks to,
; and `.sql` was indexed as searchable text with no definitions at all — so
; `definitions users` missed the table that half the codebase reads.
;
; The anchors are load-bearing. `create_trigger` has *three* `object_reference`
; children (the trigger, the table it fires on, the function it executes) and
; `column_definition` has two identifiers when the column carries a `REFERENCES`
; clause, so an unanchored pattern names each definition several times over. `.`
; after the introducing keyword (or at the start of the child list) pins the
; capture to the one that is the declaration's own name.

(create_table (keyword_table) . (object_reference) @name) @table
(create_view (keyword_view) . (object_reference) @name) @view
(create_materialized_view (keyword_view) . (object_reference) @name) @view
(create_index (identifier) @name) @index
(create_trigger (keyword_trigger) . (object_reference) @name) @trigger
(create_function (keyword_function) . (object_reference) @name (function_body)? @body) @function
(create_type (keyword_type) . (object_reference) @name) @type
(create_schema (identifier) @name) @module
(column_definition name: (_) @name) @field
