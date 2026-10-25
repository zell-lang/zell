# zell-lsp: LSP 3.17 conformance

Audit of `zell-lsp` (`lsp/`) against the Language Server Protocol 3.17.
Status: **ok** (implemented, tested), **partial** (works, with the gap
named), **missing** (not implemented), **n.a.** (does not apply to Zell or
to a server). Evidence is a test in `lsp/tests/server.rs`,
`lsp/tests/server/protocol.rs` or `lsp/tests/server/conformance.rs`
(`server::<name>`), or a source location. Method table and design notes:
`docs/TOOLING.md` "LSP-сервер".

Summary: **54 ok, 7 partial, 12 missing, 8 n.a.** (81 rows).

## Lifecycle and base protocol

| Method / rule | Status | Evidence | Gap |
|---|---|---|---|
| `initialize` / `initialized` | ok | `capabilities_advertise_the_protocol_features` | — |
| `positionEncoding` negotiation (`utf-8`, `utf-16`, `utf-32`) | ok | `positions_follow_the_negotiated_encoding` (é, 😀, CJK before the symbol: definition, hover, rename, semantic tokens, didChange) | — |
| Requests before `initialize` → `ServerNotInitialized` (−32002) | ok | `lifecycle_follows_the_specification` | — |
| `shutdown`; later requests → `InvalidRequest` | ok | `lifecycle_follows_the_specification` | — |
| `exit` without `shutdown` → exit code 1 | ok | `lifecycle_follows_the_specification` (`run` returns an error, `main.rs` exits 1) | — |
| `$/cancelRequest` | ok | `cancel_request_answers_a_waiting_request`, `errors_use_the_protocol_codes` (unknown / answered ids ignored) | Handlers run synchronously on the main thread, so only requests waiting for an analysis can be cancelled; a running handler always answers. |
| Unknown method → `MethodNotFound` | ok | `errors_use_the_protocol_codes` | — |
| Malformed params → `InvalidParams` | ok | `errors_use_the_protocol_codes` | — |
| Handler panic → `InternalError`, server keeps running | ok | `handlers.rs` `call` (`catch_unwind`) | No test (no known panicking input). |
| `$/setTrace`, `$/logTrace` | missing | — | Ignored; the server has no trace output of its own. |
| `window/workDoneProgress/create`, `$/progress` | ok | `progress_is_reported_for_initial_indexing` | — |
| `window/logMessage`, `window/showMessage` | ok | `lib.rs` `log` | — |
| `window/showDocument` | n.a. | — | Nothing to show. |
| `client/registerCapability` (dynamic registration) | missing | — | File watchers are not registered by the server: clients must send `didChangeWatchedFiles` on their own (the VS Code client does; Neovim/Helix users must configure it). |
| `telemetry/event` | n.a. | — | — |

## Document synchronisation

| Method / rule | Status | Evidence | Gap |
|---|---|---|---|
| `didOpen` / `didChange` (incremental) | ok | `incremental_sync_matches_a_reference_model` (fuzz) | — |
| Change range spanning lines, change at the very end, positions past the line/text end | ok | `crlf_documents_and_clamped_positions` | — |
| CRLF documents (a column past the end of a `\r\n` line lands before `\r`) | ok | `crlf_documents_and_clamped_positions`; `SourceFile::offset_in` | — |
| Document versions | partial | `lib.rs` `doc_versions` | Versions are recorded (used in rename edits) but not checked: a stale or out-of-order `didChange` is applied as it comes (stdio keeps order, as in rust-analyzer). |
| `didSave` with `includeText` | ok | `crlf_documents_and_clamped_positions` | — |
| `didClose`, then requests on the closed document | ok | `closed_unsaved_documents_drop_their_diagnostics_and_answer_null` (null, no panic) | — |
| `willSave`, `willSaveWaitUntil` | missing | — | Format-on-save goes through `textDocument/formatting`. |
| `notebookDocument/*` | n.a. | — | — |

## Language features

| Method | Status | Evidence | Gap |
|---|---|---|---|
| `textDocument/hover` | ok | `hover_shows_signature_and_doc`, `hover_renders_doc_markdown_and_std_examples` | — |
| `textDocument/definition` | ok | `definition_of_method_call`, `definition_and_hover_reach_into_std_sources` (returns `file://` URIs of `std/` sources) | — |
| `textDocument/declaration` | ok | `implementation_type_definition_and_declaration` | Same as definition (Zell has no separate declarations). |
| `textDocument/typeDefinition` | ok | `implementation_type_definition_and_declaration` | — |
| `textDocument/implementation` | ok | `implementation_type_definition_and_declaration` | — |
| `textDocument/references` | ok | `references_and_rename_across_files`, `dependencies_in_zell_deps_are_read_only_project_files` | — |
| `textDocument/documentHighlight` | ok | `workspace_symbols_and_highlights` | — |
| `textDocument/documentSymbol` | ok | `document_symbols_are_hierarchical` | — |
| `textDocument/prepareRename` | ok | `rename_is_versioned_null_where_nothing_and_refuses_clashes` (null where there is no symbol), `rename_refuses_std_symbols` (error with the reason) | — |
| `textDocument/rename`: one `WorkspaceEdit` across files | ok | `references_and_rename_across_files` | — |
| rename: `documentChanges` with versions when supported | ok | `rename_is_versioned_null_where_nothing_and_refuses_clashes` | — |
| rename of a symbol declared in an unopened file | ok | `rename_is_versioned_null_where_nothing_and_refuses_clashes` | — |
| rename that would clash / shadow | partial | `rename_is_versioned_null_where_nothing_and_refuses_clashes` | Refused when the new name is a sibling member or a name visible at a use (local declared earlier in the function, namespace member, import). Not detected: a local declared *later* in the same block that a use would then capture, and clashes inside other functions' block scopes (the index has no block scopes). |
| `textDocument/completion` | partial | `completion_members_and_scope` | No `textEdit` (clients use their word range; correct in VS Code with the contributed `wordPattern`), no `sortText` (client-side fuzzy order), no snippets (so `snippetSupport` is never violated), `isIncomplete` is always false and the list is never truncated. Members after `.` only for a chain of names: `a.b().` and chains split across lines (`x\n    .y.`) give no items (needs typing of the expression at the cursor). |
| `completionItem/resolve` | ok | `completion_resolve_adds_documentation` | — |
| `textDocument/signatureHelp` | ok | `signature_help_shows_parameters_and_doc` | Parameter label offsets follow the negotiated encoding. |
| `textDocument/inlayHint`, `inlayHint/resolve` | ok | `inlay_hints_types_and_parameter_names`, `inlay_hint_and_code_action_resolve` | — |
| `textDocument/codeAction`, `codeAction/resolve` | ok | `code_actions_fix_structured_diagnostics`, `inlay_hint_and_code_action_resolve`, `inspect::*` | Quick fixes (errors and lints `L0001`–`L0010`), extract variable/function, inline variable, organize imports; refactorings resolve lazily with `resolveSupport`. Missing: inline function, implement interface, change signature. |
| `textDocument/codeLens`, `codeLens/resolve` | ok | `code_lenses_for_tests_main_and_references` | — |
| `textDocument/semanticTokens/full` | ok | `semantic_tokens_are_classified_and_stable` | — |
| `semanticTokens/full/delta` | ok | `semantic_tokens_delta_and_range` (edits applied client-side equal a fresh `full`) | — |
| `semanticTokens/range` | ok | `semantic_tokens_delta_and_range` (equals `full` restricted to the range) | — |
| Semantic token legend | ok | `features.rs` `SEMANTIC_TOKEN_TYPES` / `_MODIFIERS`; only these indices are emitted | — |
| `textDocument/foldingRange` | ok | `folding_ranges_cover_blocks_and_comments` | — |
| `textDocument/selectionRange` | ok | `selection_ranges_expand_from_the_token_to_the_file` | — |
| `textDocument/formatting`, `rangeFormatting` | ok | `formatting_whole_document_and_range` | — |
| `textDocument/onTypeFormatting` | missing | — | Cheap with `zell::fmt` over the enclosing block (`}` / newline); not done. |
| `textDocument/prepareCallHierarchy`, `callHierarchy/incomingCalls`, `outgoingCalls` | ok | `call_hierarchy_incoming_and_outgoing` | A call is a reference followed by `(`; function values passed around are not calls. |
| `textDocument/prepareTypeHierarchy`, `typeHierarchy/*` | missing | — | Structs satisfy interfaces structurally; the satisfaction check exists only inside `nav.rs` (implementation) and is not indexed. Needs a struct ↔ interface table in `sema/index.rs`. |
| `textDocument/documentLink` | missing | — | `import a.b` names a namespace that can span many files; a link would need a target per import (the namespace directory or its first file). |
| `textDocument/linkedEditingRange` | n.a. | — | No paired tags in Zell. |
| `textDocument/moniker` | n.a. | — | No cross-repository index format. |
| `textDocument/inlineValue` | missing | — | Debugger integration goes through the DAP agent. |
| `textDocument/documentColor`, `colorPresentation` | n.a. | — | — |

## Diagnostics

| Method / rule | Status | Evidence | Gap |
|---|---|---|---|
| `textDocument/publishDiagnostics` (push) | partial | `error_is_reported_with_range_and_cleared` | `version` is not set. |
| `textDocument/diagnostic` (pull), push XOR pull | ok | `pull_diagnostics_for_documents_and_the_workspace` | — |
| `workspace/diagnostic` | partial | `pull_diagnostics_for_documents_and_the_workspace` | No partial results (`partialResultToken` ignored); unchanged reports are parked until an analysis changes something. |
| `workspace/diagnostic/refresh` | ok | `pull_diagnostics_for_documents_and_the_workspace` | — |
| Diagnostics cleared on `didClose` of an unsaved new file | ok | `closed_unsaved_documents_drop_their_diagnostics_and_answer_null` | — |
| `relatedInformation` | partial | `lib.rs` `to_lsp_diag` | Notes with a span become related information in the same file; notes without a span are appended to the message. |
| `tags` (`Unnecessary`) | ok | `unused_code_is_tagged_unnecessary` | `Deprecated` n.a. (no deprecation attribute yet). |
| `data` for quick fixes | partial | `code_actions_fix_structured_diagnostics` | Fixes are recomputed from the code and range; `data` is not sent (works with every client, costs a lookup). |
| `codeDescription` | missing | — | No online error index to link to. |

## Workspace

| Method / rule | Status | Evidence | Gap |
|---|---|---|---|
| `workspace/symbol` (substring + fuzzy subsequence) | ok | `workspace_symbols_and_highlights` | — |
| `workspaceSymbol/resolve` | n.a. | — | Locations are always sent. |
| `workspace/didChangeWatchedFiles` (file edited outside the editor) | ok | `watched_files_are_added_and_removed` | See dynamic registration above. |
| `workspace/didChangeConfiguration` | ok | `inlay_hints_types_and_parameter_names` | Push only. |
| `workspace/configuration` (pull) | missing | — | Settings come from `initializationOptions` and `didChangeConfiguration`. |
| `workspace/didChangeWorkspaceFolders` | missing | — | Folders are read once at `initialize`; adding a folder needs a restart. |
| `workspace/willRenameFiles` (edits imports and the namespace line) | ok | `renaming_a_file_updates_its_namespace_and_imports`, `renaming_a_file_of_a_shared_namespace_adds_imports` | — |
| `workspace/didRenameFiles` | ok | same tests | — |
| `workspace/willCreateFiles` etc., `didCreateFiles`, `didDeleteFiles` | missing | — | Covered by watched files. |
| `workspace/executeCommand` | n.a. | — | Code lens commands run in the client. |
| `workspace/semanticTokens/refresh`, `inlayHint/refresh`, `codeLens/refresh` | missing | — | After an edit in one file, other open files' tokens/hints/lenses update only when the client asks again. |

## Robustness

| Check | Status | Evidence | Gap |
|---|---|---|---|
| 20k-line file: analysis + full semantic tokens < 1 s | ok | `robustness_large_files_edit_storms_and_broken_syntax` (~0.1 s for tokens in a debug build) | — |
| 200 rapid `didChange`: no backlog (`zell/analyzerStatus`) | ok | same test (< 20 analyses started, 0 queued) | — |
| Syntax error at every nesting level: no panic, positioned diagnostics, queries answer | ok | same test, `queries_work_in_a_file_with_syntax_errors` | — |
| Every example under every query | ok | `every_example_survives_all_queries` | — |

## Still missing for a gopls / rust-analyzer level server

Ranked by value for the cost (S < 1 day, M 1–3 days, L > 3 days):

1. **Completion quality** (M–L): typing of the expression at the cursor
   for members after calls and multi-line chains, `textEdit` with the
   replaced range, `sortText` by relevance (locals > members > globals >
   keywords), call snippets with `snippetSupport`, auto-import items.
2. **Incremental analysis** (L): every edit re-analyses the whole project
   (debounced and cancellable, fine at today's sizes); per-namespace
   invalidation is needed for large workspaces.
3. **Handlers off the main thread** (M): long requests (references and
   workspace symbols on a big project) block other requests and cannot be
   cancelled once running.
4. **Dynamic registration + workspace folders + `workspace/configuration`**
   (S): watchers registered by the server, folders added at run time,
   settings pulled — needed for non-VS Code clients.
5. **Refresh requests** (S): `semanticTokens/refresh`, `inlayHint/refresh`,
   `codeLens/refresh` after an analysis that changed other open files.
6. **Type hierarchy** (M): index struct ↔ interface satisfaction.
7. **Rename scoping** (M): block scopes in the index to detect every
   shadowing case.
8. **Refactoring code actions** (M each): extract variable/function,
   inline variable and organize imports are done (`lsp/src/refactor.rs`);
   still missing: inline function, fill struct fields, implement
   interface stubs, change signature, `if err` <-> `?` conversion.
9. **`onTypeFormatting`, `documentLink`** (S each).
10. **Diagnostics details** (S): `version` in `publishDiagnostics`,
    `data` for quick fixes, cross-file related information, partial
    results for `workspace/diagnostic`.
