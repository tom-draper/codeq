; Protobuf: the interface between services in a polyglot repo, so a message name
; is often the one symbol that crosses language boundaries.

(message (message_name) @name) @class
(enum (enum_name) @name) @enum
(service (service_name) @name) @interface
(rpc (rpc_name) @name) @method
(field (identifier) @name) @field
(enum_field (identifier) @name) @variant
(import (string) @name) @import
