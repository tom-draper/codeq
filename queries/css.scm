; CSS. Rules are not "definitions" in the usual sense, but "where is `.primary`
; styled?" is a real question with an exact answer, so a class/id selector and a
; named at-rule are captured. The bigger win is spans: `<style>` inside a Svelte
; or Vue component was previously opaque, so CSS comments read as code.

(rule_set (selectors (class_selector (class_name) @name))) @type
(rule_set (selectors (id_selector (id_name) @name))) @type
(keyframes_statement (keyframes_name) @name) @type
(import_statement (string_value) @name) @import
