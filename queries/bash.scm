; Bash: functions only (source/. includes are plain commands, not captured).

; `source ./lib.sh` and `. ./lib.sh` — a command, guarded on its name.
(command
  name: (command_name (word) @guard)
  argument: (word) @name) @import

(function_definition name: (word) @name body: (_) @body) @function
