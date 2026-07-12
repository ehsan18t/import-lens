# A runtime is an artifact boundary

An import resolves under a runtime — Server, Client, or Component — and a single document can
mix them (Astro frontmatter is Server; a client script is Client). Those are **two things that
ship**, each carrying its own copy of anything both need. So costs are measured per runtime
and *added* across runtimes, and nothing is ever deduplicated across a runtime boundary.

Two behaviours followed from never having stated this:

- File sizing built one bundle per runtime (correctly — they resolve under different
  conditions) and then **joined the minified outputs and compressed the concatenation once**.
  That compresses away redundancy between two artifacts that ship separately, so the reported
  compressed size was a strict *lower bound* on what actually ships, with no diagnostic. It now
  feeds the per-file budget ([ADR-0004](0004-import-lens-measures-imports-not-bundles.md)), so
  each runtime group is compressed on its own and the results are summed.
- Shared-module accounting counted a module as shared if it appeared in more than one result,
  **with no runtime partition** — so a package imported from both frontmatter and a client
  script was reported as a shared dependency, and the UI sold the user a deduplication saving
  the build model explicitly does not perform. Sharing is now computed within a runtime only.

## Note on summing compressed bytes

[ADR-0004](0004-import-lens-measures-imports-not-bundles.md) forbids summing compressed sizes,
because compression is not linear. Summing *across* an artifact boundary is the exception, and
the reason is precisely why the boundary exists: the two payloads really are compressed
separately in the real world, so adding their compressed sizes models reality rather than
distorting it. The same rule extends to non-JavaScript assets, which are separate artifacts
from the JavaScript chunk.

## Consequences

Reported sizes for mixed-runtime files go **up**, and some "shared dependency" tooltips
disappear. Both are corrections, but a user watching a number will read them as a regression.
