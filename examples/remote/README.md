# Remote applets

execute.applet/1 is a version-pinned HTTPS protocol for a single declared
applet. It is not MCP. The host fetches GET /.well-known/execute-applet/v1
only when discovery is explicitly enabled, validates the typed descriptor,
then invokes POST /invoke. Event outputs use GET /events/{port} with
declared SSE or JSON Lines framing.

Remote applet manifests use backend: remote, binary: @remote-applet, and a
contract.remote endpoint configuration. Bearer credentials come only from an
explicitly named environment variable or file. Static checks never contact
the endpoint: they can compare the local typed interface with the
SHA-256-pinned descriptor cache below. A runtime may discover when
allow_discovery is explicitly true; otherwise it requires that cache.

The cache path is relative to the manifest directory. Policy-enabled hosts
authorize the resolved file read before loading it. The digest is the
lowercase SHA-256 of the exact descriptor file bytes. Regenerate it only after
reviewing a new descriptor.

Example manifest fields:

    {
      "name": "demo.echo",
      "binary": "@remote-applet",
      "parameters": {},
      "arguments": [],
      "contract": {
        "version": 1,
        "backend": "remote",
        "remote": {
          "endpoint": "https://applets.example.invalid",
          "auth": {"kind": "bearer_env", "name": "DEMO_ECHO_TOKEN"},
          "descriptor_cache": "demo.echo.descriptor.json",
          "descriptor_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        },
        "ports": {
          "inputs": {"name": {"schema": {"type": "text"}, "delivery": "single"}},
          "outputs": {"result": {"schema": {"type": "json"}, "delivery": "single"}}
        }
      }
    }

The transport sends bounded compact JSON. Binary input and output bytes use
base64; typed values remain JSON and are checked against the shared canonical
contract schemas. Request IDs are echoed by the service and validated by the
client. HTTP redirects and proxy environment settings are disabled. HTTP is
accepted only for explicitly enabled loopback fixtures.

Bearer-file credentials are accepted on Unix only when the opened file is a
regular file owned by the current user with no group/world permission bits.
Windows currently rejects bearer-file credentials because the host cannot yet
verify the privacy ACL of an existing file; use an explicitly named
`bearer_env` source there, subject to the capability policy's environment
allowlist. Relative bearer-file paths resolve from the manifest directory.

POST requests are sent once. A timeout, disconnect, malformed response or
request-ID mismatch after sending is reported as an uncertain effect; it is
never automatically replayed. A server may declare a narrow operation and
scope for explicit idempotency keys. Event delivery is pull-based and
at-least-once. Resumption requires a declared cursor; reconnect is opt-in and
finite, sends Last-Event-ID, and may surface duplicate cursors to the caller.
This transport does not claim to deduplicate remote effects.

The no-automatic-retry rule follows [RFC 9110 section 9.2.2](https://www.rfc-editor.org/rfc/rfc9110.html#section-9.2.2):
POST is not inherently idempotent. Cursor resumption uses the
[WHATWG Server-Sent Events Last-Event-ID mechanism](https://html.spec.whatwg.org/multipage/server-sent-events.html#the-last-event-id-header).
The transport uses nonblocking sockets and checks cancellation during DNS
helper supervision and socket readiness polls at intervals up to 20 ms. The
configured finite timeout remains the upper bound for each network phase.
The execute CLI supervises its hidden DNS helper as a child process, clears
its environment, and kills and reaps it on cancellation or timeout. Embedded
library users that do not configure an explicit resolver helper may use
literal-IP endpoints; hostname resolution fails closed.

The interruptible socket adapter uses ureq's explicitly unstable
`unversioned::transport` API, so the ureq dependency is pinned to 3.4.2.

descriptor.json is an example of a typed version-1 discovery document.
invoke.request.json and invoke.response.json show a request/response pair.
