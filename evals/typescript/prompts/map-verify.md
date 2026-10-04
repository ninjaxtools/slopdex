Make one compact discovery pass, then follow the implementation:

`slopdex map --private --ignore-errors -k fns -g '*.go' -g '!**/*_test.go' -i -e '<name-term|other-name-term>' [paths...]`

- Use distinctive fragments from the task, OR-ed in one name regex. `-e` matches names and qualified names, not body text. Scope to a likely subsystem when clear; otherwise use returned paths to discover it.
- Once a plausible entry point is mapped, stop inventory browsing and read it. Follow concrete names found in source. Use a scoped map only when a needed declaration's location is unknown; grep is for body text and unresolved usages.
- Batch independent lookups and reads. Keep reads to relevant implementations; do not repeat declaration checks, map after finishing source verification, or read whole files to learn their functions.
- One empty map warrants a broader selector; noisy results warrant narrower paths/terms. Avoid unrestricted `.*` maps and head/tail inventory splits.
- For every critical explanation claim, check the exact predicate and ordering in source. Read complete short routines, including cleanup outside branches, and relevant construction/disposal contracts. Distinguish missing data from sentinel values; qualify claims you cannot verify.
- The checkout and index are ready. Start directly with discovery; skip Git/layout/help/index troubleshooting. Verify the required behavior and citations in source, then return the task's exact JSON schema.
