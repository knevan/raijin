# Native HLS VOD downloads

## Decision and scope

Raijin downloads HLS media natively. It does not require FFmpeg, launch an external media tool, transcode, or pretend that renaming a transport stream produces MP4.

The initial target is finite, unencrypted MPEG-TS HLS VOD, approaching ABDM's native segment-download scope with stricter validation and recovery. AES-128 is deferred to a later iteration. Rust crates are acceptable where they provide necessary parsing or integrity primitives; an external media runtime is not part of this phase.

## Entry points and selection

- Desktop Add Download recognizes HTTP(S) URLs whose path ends in `.m3u8`, including URLs with query tokens.
- The local API accepts explicit `type: "hls"`, including extensionless playlist URLs. Confirmation preserves this type while the original URL is unchanged.
- Plain HTTP downloads remain plain HTTP; a direct MP4 file does not need HLS processing.
- Output names use `.ts`. Playlist `Content-Length` and browser size hints are not the final media size.
- Master playlists select the highest-bandwidth supported variant deterministically. Manual quality selection is deferred; the chosen media playlist is persisted and resume does not silently select another variant.

## Supported boundary

Supported jobs contain a finite media playlist ending in `EXT-X-ENDLIST`, with ordinary MPEG-TS segment resources and no separate external audio/video tracks.

The initial implementation rejects encryption, live/sliding playlists, fragmented MP4 initialization maps, byte-range segments, discontinuities, gaps, and I-frame-only streams rather than silently producing incomplete media. An extension alone is not proof of MPEG-TS: segment packet framing is checked. This is structural validation, not a decoder or proof of audiovisual correctness.

Separate audio/video muxing, subtitles, DASH, Low-Latency HLS, website extraction, automatic signed-URL refresh, transcoding, and MP4 output are outside this iteration. Browser media discovery is also separate from accepting an explicitly supplied playlist.

## Architecture

Queue admission remains the only orchestration path. `DownloadManager` dispatches HTTP and HLS jobs independently. HLS does not reuse the HTTP byte-range partition model or spawn an HTTP multi-connection job for each segment.

The HLS job resolves and validates playlists, persists a selected segment plan, executes bounded segment workers, then assembles the completed resources in order. Existing speed limiters and monitor events are reused. The job occupies its queue slot through assembly and publication, not just network transfer.

SQLite stores the plan and segment completion metadata. Segment payloads are temporary files, not independent state sidecars. No JSON state files are introduced.

## Recovery and publication

- Completed segments are retained across pause and restart; incomplete segments restart from their beginning.
- Completed payloads are checked against persisted length and SHA-256 before reuse. Missing or damaged files are downloaded again; they are not silently assembled.
- Partial resume validates the selected remote plan and requires a matching ETag or Last-Modified for retained segments. Equal length alone does not establish identity. Sources without usable validators require an explicit reset rather than silently mixing old and fresh bytes.
- If every segment is complete and passes local integrity checks, assembly retries do not contact the source; an expired URL does not prevent publishing already downloaded content.
- Local hashes establish local integrity, not remote immutability. Server validators improve remote identity checks but cannot guarantee a server has not changed content while reusing validators.
- Workers stop before reset/removal cleans their artifacts. Stale completion notifications cannot remove a newer job instance.
- Assembly writes a newly created incomplete destination before non-overwriting final publication. An old incomplete pathname is unlinked before writing so a leftover hard link cannot truncate an already published output. Segments remain available if assembly fails.
- Publication currently requires filesystem hard-link support. Unsupported filesystems fail safely and retain the segments instead of falling back to a potentially overwriting rename. NTFS is covered by the local tests; other filesystem/platform combinations require acceptance testing.
- A crash after publishing but before recording completion can leave an extra output. Recovery chooses a numbered filename instead of claiming ownership of a pre-existing file, even if its content is identical. Transactional publication recovery is deferred.
- Assembly may temporarily require approximately twice the media size in disk space. Incremental deletion while assembling was rejected because it weakens recovery.

Network bytes are actual transferred/persisted media bytes, not an estimate from variant bandwidth. Total size and ETA stay unknown until a reliable total is available. Resolving/downloading/assembly states must not imply that downloading a playlist has completed the media job.

## Security and limits

Playlist reads, nesting, redirects, worker concurrency, and retries are bounded. Relative resources resolve against the final containing playlist URL. Captured credentials are scoped to their original origin; references and redirects do not authorize forwarding them to unrelated hosts. Query parameters are not copied between unrelated URIs. HTTP(S) is the only network resource scheme.

Temporary file names derive from application-controlled identifiers, never remote paths. Diagnostics must not include signed URLs, cookies, authorization headers, or keys.

## Deferred decisions

MP4 output, if required later, needs a separately evaluated muxing implementation and codec/container compatibility tests. Do not add a processor registry, external-tool downloader, or speculative muxer abstraction before that requirement exists.
