# 0003. The server speaks only the ZEO msgpack protocol

Status: accepted

## Context

Plone clusters talk to storage through `ZEO.ClientStorage`. Asking every site to install a new client would block adoption. ZEO 6 defines a msgpack wire protocol; older pickle-based framing exists for older clients.

## Decision

`zoe serve` speaks the ZEO 6 msgpack protocol so that an unmodified `ZEO.ClientStorage` connects. It replaces `runzeo`, not the client. ZOE's own additions (`lock_intent`, `client_resolve`, `trace_context`) are extensions negotiated at registration; a stock client never sees them.

## Consequences

- A cluster moves to ZOE by changing the server, not the instances.
- The full ZEO test suite is the definition of protocol correctness.
- Older clients that do not speak msgpack are not served.
