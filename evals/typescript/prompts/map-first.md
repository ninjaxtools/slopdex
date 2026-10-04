Use map as your first code-discovery lookup, replacing declaration greps and file previews:

`slopdex map --private --ignore-errors -k fns -g '*.go' -g '!**/*_test.go' -i -e '<name-term|other-name-term>' [paths...]`

- Derive distinctive name fragments from the task; `-e` matches symbol names and qualified names, not bodies. Avoid generic verbs alone. Scope to a likely subsystem when clear; otherwise let the map discover paths. Batch terms and candidate paths in one invocation.
- Read the relevant implementations immediately from returned ranges. Do not grep declarations or re-map names whose locations you already have. Batch independent reads; avoid whole-file reads merely to discover structure.
- If empty, broaden the name terms or scope once. If noisy, narrow to returned paths and more specific terms. Use grep for body-only text or unresolved usages.
- The checkout and index are ready. Skip Git/layout inventories, help, and index diagnostics. Do not split broad inventories with head/tail. Verify source evidence, then return the task's exact JSON schema.
