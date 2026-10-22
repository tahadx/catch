# Segmented downloads

Catch can download a single large file through several parallel HTTP requests,
each fetching a distinct byte range. One worker still owns the queue entry and
writes every range into its assigned position in one `.part` file. The finished
file is renamed into place only after all ranges are durably committed, so the
result is the same file a sequential download would produce.

Segmentation is automatic. When a server cannot prove that it supports real
byte ranges, Catch uses its ordinary sequential downloader for that file.

## When Catch segments a file

All of these must hold:

- The file is at least **32 MiB**.
- The server answers a `Range: bytes=0-1023` probe with `206 Partial Content`
  and a valid `Content-Range` that describes the requested bytes and the full
  length.
- The response uses identity content encoding, so byte offsets are stable.
- The server exposes an `ETag` or `Last-Modified` validator. Catch refuses to
  splice ranges when it cannot tell whether the representation changed.
- At least two connections are available. Catch divides a global connection
  budget across active transfers, caps each transfer at the **Connections per
  file** setting, and sizes ranges around **8 MiB**.
- The transfer is starting from byte zero. An existing partial file continues
  through the sequential path instead of being reinterpreted as segments.
- **Connections per file** is not set to 1. Values are 1, 2, 4, or 8; the
  default is 4.

## When a server does not cooperate

Catch falls back to a fresh sequential download from byte zero when the server:

- ignores `Range` and answers `200 OK`;
- sends a mismatched or malformed `Content-Range`;
- changes its validator or the representation length between requests;
- rejects a range with HTTP 416; or
- sends a content encoding that makes byte offsets unstable.

Bytes from two different representations are never mixed.

## Resume and recovery

A sidecar manifest named `<file>.<id>.part.meta` records which ranges are
durably committed. A range is committed only after its bytes are written and
synchronized. After an interruption or a restart, completed ranges are reused
and only the missing ones are fetched again.

An existing partial file is never mistaken for a segmented one:

- With no manifest, the part is treated as an ordinary sequential partial and
  resumed sequentially.
- With a corrupt manifest, an incompatible manifest, a different URL, or a
  part length that disagrees with the manifest, the part is moved aside as
  `<file>.<id>.part.quarantined-<seconds>` and the transfer starts over.

Removing a download deletes both the part and its manifest.

## Work stealing

While a segmented download runs, a connection that finishes its range early
does not sit idle. Catch hands part of the slowest remaining range to it, but
only when that range is larger than the minimum piece. The split point is the
exact byte frontier already written, never a guess, so the slowest segment does
not set the finish time alone.

## Diagnostics

While a segmented download is running, the row shows `done/total seg`.
`catch why <id>` prints what the transfer is doing, the partial data on disk,
segment progress, validators, and retry state. Use `catch list` to find the id.

The manifest is ordinary JSON, next to the partial file:

```bash
cat ~/Downloads/catch/<file>.<id>.part.meta   # default folder
```

A segment that exhausts its retries is re-queued once on a fresh connection
before the transfer fails. Partial data stays on disk for a manual retry either
way.

## Environment overrides

All variables are optional. The panel settings are the normal way to change
these values.

| Variable | Default | Meaning |
| --- | --- | --- |
| `CATCH_SEGMENTS` | 4, or the panel setting | Segments per eligible file (1, 2, 4, or 8; 1 disables) |
| `CATCH_SEGMENT_MIN_BYTES` | 33554432 (32 MiB) | Smallest file eligible for segmentation |
| `CATCH_SEGMENT_MIN_PIECE` | 8388608 (8 MiB) | Target segment size, used to choose the range count |
| `CATCH_MAX_CONNECTIONS` | parallel × segments, or the traffic mode | Global connection budget across transfers (1–64) |
| `CATCH_MAX_PER_HOST` | global budget | Per-host connection ceiling (1–64) |
| `CATCH_BW_MODE` | `unlimited` | `unlimited`, `balanced`, or `low`: the connection budget the settings add up to, halved, or quartered |
| `CATCH_BW_CAP_BPS` | none | Global download cap in bytes/second |
| `CATCH_MAX_PARALLEL` | 3 | Parallel transfers for headless use (1–10) |

`CATCH_MAX_PARALLEL` and `CATCH_SEGMENTS` also decide the default connection
budget. `CATCH_MAX_CONNECTIONS` overrides that budget; `CATCH_BW_CAP_BPS` can
reduce the per-file slices further.
