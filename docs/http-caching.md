# HTTP forwarding and caching

The proxy forwards end-to-end request headers and bodies and uses the configured
upstream authority for Host. Hop-by-hop headers, including fields named by
Connection, are removed in both directions. Cache hits receive the same filtering
as misses, and Colander owns its X-Cache, X-Cache-Policy and X-Mode headers.

Only ordinary GET requests are eligible for shared caching. Authorization,
Cookie, Range, conditional requests, requests with bodies, and request cache
control directives bypass lookup and storage. Responses with Set-Cookie, Vary,
Content-Range, private, no-cache or no-store bypass storage. Vary is conservatively
bypassed until variant-aware keys are implemented. Host and the complete request
URI separate cached representations. Successful unsafe methods invalidate the
GET entry for the requested URI in both policies.

Repeated Cache-Control fields are combined. s-maxage takes precedence over
max-age regardless of directive order. Invalid or duplicate freshness directives
prevent caching. Upstream Age reduces remaining freshness; cache hits increase
Age with their residence time. This is a conservative cache, not a complete RFC
9111 implementation: validators, full age calculations, related-resource
invalidation and stale revalidation are not implemented.

upstream.timeout_ms bounds receiving upstream headers and reading response bodies.
Timeouts before response headers are returned produce 504; a timeout during a
streamed body terminates that body. Non-cacheable responses and responses larger
than max_body_size_bytes stream through without storing the entire body. Candidate
responses are buffered only to the cache limit plus the current HTTP data frame.
