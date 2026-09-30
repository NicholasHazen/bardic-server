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

## Listeners (M1)

```sh
# Add yourself, then use the id as X-Bardic-Listener on listener-scoped calls
curl -s -X POST -H "$D" -H "$J" -d '{"name":"Nick"}' $B/api/listeners
L="X-Bardic-Listener: <id from above>"
curl -s $B/api/listeners
curl -s $B/api/listeners/<id>/settings
curl -s -X PUT -H "$D" -H "$J" -d '{"default_voice_id":null,"place_conflict":"ask","continue_into_next_chapter":true}' $B/api/listeners/<id>/settings
```

## Library (M1)

```sh
# Add a book: EPUB or UTF-8 text. Returns 202 at once; poll the import.
curl -s -X POST -H "$D" -F "file=@/path/to/book.epub" $B/api/imports
curl -s $B/api/imports/<import id>          # state: queued ... done | failed | cancelled
curl -s -X DELETE -H "$D" $B/api/imports/<import id>   # cancel: leaves no book

# Check a file before uploading it (SHA-256 of the file), and add the built-in sample
curl -s -H "$D" "$B/api/books/duplicates?sha256=$(shasum -a 256 book.epub | cut -d' ' -f1)"
curl -s -X POST -H "$D" -H "$J" -d '{}' $B/api/books/sample

# The library (needs the listener header)
curl -s -H "$D" -H "$L" "$B/api/books?sort=title&limit=20"
curl -s -H "$D" -H "$L" "$B/api/books?q=ferry"
curl -s -H "$D" -H "$L" $B/api/series

# One book: chapters, exact text with line spans (code points), search, cover
curl -s $B/api/books/<id>/chapters
curl -s $B/api/books/<id>/chapters/<chapter id>/text
curl -s "$B/api/books/<id>/search?q=ferryman"
curl -s -o cover.jpg $B/api/books/<id>/cover

# Edit, remove, restore (text is never changed)
curl -s -X PATCH -H "$D" -H "$J" -d '{"title":"New title","series":{"name":"The Cycle","order":2}}' $B/api/books/<id>
curl -s -X POST -H "$D" $B/api/books/<id>/remove
curl -s -X POST -H "$D" $B/api/books/<id>/restore
```

## Places (M2)
```sh
L="X-Bardic-Listener: <listener id>"
BOOK=<book id>; CH=<chapter id>
# Where am I? 404 place_not_found if the book was never opened
curl -s -H "$L" $B/api/books/$BOOK/place
# Write the place. base_revision is the revision you last saw (0 if none).
curl -s -X PUT -H "$D" -H "$L" -H "$J" -d "{\"chapter_id\":\"$CH\",\"offset\":120,\"mode\":\"listening\",\"base_revision\":0}" $B/api/books/$BOOK/place
# From a second device with a stale revision you get 409 place_conflict with server_place
# Earlier places, marking finished, and starting over
curl -s -H "$L" $B/api/books/$BOOK/place/history
curl -s -X PUT -H "$D" -H "$L" -H "$J" -d '{"finished":true}' $B/api/books/$BOOK/place/finished
curl -s -X DELETE -H "$D" -H "$L" $B/api/books/$BOOK/place
# The library with your progress
curl -s -H "$L" "$B/api/books?filter=in_progress&sort=recent"
```

## Voices and audiobooks (M3a)
```sh
# Set up your Breeze server (tested first; nothing is stored if it fails)
curl -s -X PUT -H "$D" -H "$J" -d '{"base_url":"http://host.local:7860"}' $B/api/voice-sources/breeze
curl -s $B/api/voice-sources
curl -s "$B/api/voices?tier=free"
curl -s -X POST -H "$D" $B/api/voice-sources/breeze/refresh
# An audiobook is a book in one voice; creating it is free and makes no audio yet
curl -s -X POST -H "$D" -H "$J" -d '{"voice_id":"<voice id>"}' $B/api/books/$BOOK/audiobooks
curl -s $B/api/audiobooks/<audiobook id>/chapters
```

## Making and playing audio (M3b)
```sh
AB=<audiobook id>; CH=<chapter id>
# Press play: makes the chapter and the next one (202 with a job), or 200 if it is ready
curl -s -X POST -H "$D" -H "$L" $B/api/audiobooks/$AB/chapters/$CH/request
# Make the whole book in the background (Idempotency-Key makes a retry safe)
curl -s -X POST -H "$D" -H "$L" -H "$J" -H "Idempotency-Key: once" -d '{"scope":{"kind":"whole_book"}}' $B/api/audiobooks/$AB/make-ready
curl -s $B/api/jobs/<job id>          # progress, waiting, needs_you
curl -s -X POST -H "$D" $B/api/jobs/<job id>/pause   # or /resume, /cancel
curl -s $B/api/audiobooks/$AB/chapters               # ready / making / not_yet per chapter
# The audio supports Range; timings drive read-along
curl -s -H "Range: bytes=0-99" $B/api/audio/<audio id> -o /dev/null -D -
curl -s $B/api/audio/<audio id>/timings
curl -s $B/api/voices/<voice id>/sample -o sample.wav
```

## Browsers on another address

```sh
cargo run -p bardic-server -- --allow-origin http://localhost:5173
# A page from an address not in the list cannot change anything (403 origin_not_allowed).
```

## Errors
Every error is `{ "code": ..., "detail": ... }`. For example a change without a device:

```sh
curl -s -X PATCH -H "$J" -d '{"name":"X"}' $B/api/server
# {"code":"device_required","detail":"Send X-Bardic-Device ..."}   (400)
```

A second server on the same data folder refuses to start (exit 1); the data folder is locked while one runs.
