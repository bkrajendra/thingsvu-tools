# Vendored KiCad API protos

Copied unchanged from KiCad's `api/proto` directory at commit `d033635628`, the source of the pinned nightly `10.99.0-5896-gd033635628`:
<https://gitlab.com/kicad/code/kicad/-/tree/d033635628/api/proto>

Licenses are as stated in each file's header, kept as is: 20 files are GPL-3.0-or-later; 4 are MIT (`common/commands/library_commands.proto`, `common/commands/variant_commands.proto`, `common/types/library_types.proto`, `common/types/variants.proto`). Both are compatible with this project's GPL-3.0-or-later license.

`SHA256SUMS` lists a checksum for every file. Do not edit the files by hand. To update, replace the whole tree from the new pinned commit, regenerate `SHA256SUMS`, record the commit in `docs/kicad-api-compatibility.md`, and re-run the Phase 0 probes.
