# Trying the server with curl

The server is exercised with curl and the conformance tests; clients can test themselves against a running server. Every mutating request needs `X-Bardic-Device` (any 8 to 80 characters of letters, digits, `-` or `_`). Listener-scoped operations will also need `X-Bardic-Listener` once listeners land (M1).

```sh
cargo run -p bardic-server -- --data-dir ./data --bind 127.0.0.1:8765
B=http://127.0.0.1:8765
D="X-Bardic-Device: my-laptop-0001"
J="content-type: application/json"
```

## Available now (M0)

```sh
# Health: no headers needed
curl -s $B/api/health

# The server: name, version, contract version, limits, free space
curl -s -H "$D" $B/api/server

# Rename it (shown on every device)
curl -s -X PATCH -H "$D" -H "$J" -d '{"name":"Nick Mac mini"}' $B/api/server

# Devices that have used the server, and naming yours
curl -s -H "$D" $B/api/devices
curl -s -X PATCH -H "$D" -H "$J" -d '{"name":"Nick laptop"}' $B/api/devices/my-laptop-0001

# Who did what (newest first; filter by action; page with ?after=<next>)
curl -s -H "$D" "$B/api/audit?limit=10"
curl -s -H "$D" "$B/api/audit?action=server.renamed"

# Live changes (Server-Sent Events). Resume with Last-Event-ID.
curl -N -H "$D" $B/api/events
curl -N -H "$D" -H "Last-Event-ID: 0" $B/api/events
```

## Errors
Every error is `{ "code": ..., "detail": ... }`. For example a change without a device:

```sh
curl -s -X PATCH -H "$J" -d '{"name":"X"}' $B/api/server
# {"code":"device_required","detail":"Send X-Bardic-Device ..."}   (400)
```

A second server on the same data folder refuses to start (exit 1); the data folder is locked while one runs.
