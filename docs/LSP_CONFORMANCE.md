# zell-lsp: LSP 3.17 conformance

Audit of `zell-lsp` (`lsp/`) against the Language Server Protocol 3.17.
Status: **ok** (implemented, tested), **partial** (works, with the gap
named), **missing** (not implemented), **n.a.** (does not apply to Zell or
to a server). Evidence is a test in `lsp/tests/server.rs`,
`lsp/tests/server/protocol.rs`, `lsp/tests/server/conformance.rs`,
`lsp/tests/server/typehier.rs` or `lsp/tests/server/workspace.rs`
(`server::<name>`), or a source location. Method table and design notes:
`docs/TOOLING.md` "LSP-сервер".

Summary: **75 ok, 0 partial, 4 missing, 7 n.a.** (86 rows).

## Lifecycle and base protocol

| Method / rule | Status | Evidence | Gap |
|---|---|---|---|
| `initialize` / `initialized` | ok | `capabilities_advertise_the_protocol_features` | — |
| `positionEncoding` negotiation (`utf-8`, `utf-16`, `utf-32`) | ok | `positions_follow_the_negotiated_encoding` (é, 😀, CJK before the symbol: definition, hover, rename, semantic tokens, didChange) | — |
| Requests before `initialize` → `ServerNotInitialized` (−32002) | ok | `lifecycle_follows_the_specification` | — |
| `shutdown`; later requests → `InvalidRequest` | ok | `lifecycle_follows_the_specification` | — |
| `exit` without `shutdown` → exit code 1 | ok | `lifecycle_follows_the_specification` (`run` returns an error, `main.rs` exits 1) | — |
| `$/cancelRequest` | ok | `cancel_request_answers_a_waiting_request`, `cancelling_a_running_references_request`, `errors_use_the_protocol_codes` (unknown / answered ids ignored) | — (waiting, parked and running requests; long loops check the flag) |
| `ContentModified` (−32801) | ok | `an_edit_makes_an_in_flight_request_content_modified`, `every_request_is_answered_once_under_concurrent_edits` | — (an edit stops in-flight requests on that document; project-wide ones on any edit) |
| Unknown method → `MethodNotFound` | ok | `errors_use_the_protocol_codes` | — |
| Malformed params → `InvalidParams` | ok | `errors_use_the_protocol_codes` | — |
| Handler panic → `InternalError`, server keeps running | ok | `handlers.rs` `call` (`catch_unwind`) | No test (no known panicking input). |
| `$/setTrace`, `$/logTrace` | ok | `trace_and_stale_changes` | `initialize.trace` or `$/setTrace`; traces each request's and analysis's time and dropped stale changes (`verbose` adds nothing yet). |
| `window/workDoneProgress/create`, `$/progress` | ok | `progress_is_reported_for_initial_indexing` | — |
| `window/logMessage`, `window/showMessage` | ok | `lib.rs` `log` | — |
| `window/showDocument` | n.a. | — | Nothing to show. |
| `client/registerCapability` (dynamic registration) | ok | `file_watchers_are_registered_dynamically` | After `initialized`, `workspace/didChangeWatchedFiles` for `**/*.zl`, `**/zell.toml`, `**/zell.lock` when the client supports `didChangeWatchedFiles.dynamicRegistration`. Other capabilities are static. |
| `telemetry/event` | n.a. | — | — |

## Document synchronisation

| Method / rule | Status | Evidence | Gap |
|---|---|---|---|
| `didOpen` / `didChange` (incremental) | ok | `incremental_sync_matches_a_reference_model` (fuzz) | — |
| Change range spanning lines, change at the very end, positions past the line/text end | ok | `crlf_documents_and_clamped_positions` | — |
| CRLF documents (a column past the end of a `\r\n` line lands before `\r`) | ok | `crlf_documents_and_clamped_positions`; `SourceFile::offset_in` | — |
| Document versions | ok | `trace_and_stale_changes` | Recorded (rename edits, `publishDiagnostics.version`, fix `data`); a `didChange` with a lower version than the recorded one is dropped (and traced). |
| `didSave` with `includeText` | ok | `crlf_documents_and_clamped_positions` | — |
| `didClose`, then requests on the closed document | ok | `closed_unsaved_documents_drop_their_diagnostics_and_answer_null` (null, no panic) | — |
| `willSave`, `willSaveWaitUntil` | missing | — | Deliberately not advertised: clients that send `willSaveWaitUntil` (VS Code does) would then format on every save regardless of the user's format-on-save setting; format-on-save goes through `textDocument/formatting`. |
| `notebookDocument/*` | n.a. | — | — |

## Language features

| Method | Status | Evidence | Gap |
|---|---|---|---|
| `textDocument/hover` | ok | `hover_shows_signature_and_doc`, `hover_renders_doc_markdown_and_std_examples` | — |
| `textDocument/definition` | ok | `definition_of_method_call`, `definition_and_hover_reach_into_std_sources` (returns `file://` URIs of `std/` sources) | — |
| `textDocument/declaration` | ok | `implementation_type_definition_and_declaration` | Same as definition (Zell has no separate declarations). |
| `textDocument/typeDefinition` | ok | `implementation_type_definition_and_declaration` | — |
| `textDocument/implementation` | ok | `implementation_type_definition_and_declaration`, `type_hierarchy_supertypes_and_subtypes` (promoted methods, from the satisfaction table) | — |
| Type aliases: definition, hover, references, rename (`type Celsius = float`, `type P = Point` with `P{}`) | ok | `type_aliases_navigate_and_rename` | A use of an alias of a struct, enum or interface is the alias's, not the aliased type's. |
| `textDocument/references` | ok | `references_and_rename_across_files`, `dependencies_in_zell_deps_are_read_only_project_files` | — |
| `textDocument/documentHighlight` | ok | `workspace_symbols_and_highlights` | — |
| `textDocument/documentSymbol` | ok | `document_symbols_are_hierarchical` | — |
| `textDocument/prepareRename` | ok | `rename_is_versioned_null_where_nothing_and_refuses_clashes` (null where there is no symbol), `rename_refuses_std_symbols` (error with the reason) | — |
| `textDocument/rename`: one `WorkspaceEdit` across files | ok | `references_and_rename_across_files` | — |
| rename: `documentChanges` with versions when supported | ok | `rename_is_versioned_null_where_nothing_and_refuses_clashes` | — |
| rename of a symbol declared in an unopened file | ok | `rename_is_versioned_null_where_nothing_and_refuses_clashes` | — |
| rename that would clash / shadow | ok | `rename_is_versioned_null_where_nothing_and_refuses_clashes`, `rename_refuses_every_shadowing_case` | Refused with the clashing declaration and its line: a sibling member; a local of the same block (declared before or after); a local whose use would be captured by an inner block's local, or that would capture a use of another declaration of the name; a global used where a local of the new name is visible, in any function; a global hidden by a namespace member or import at a use. Block scopes come from the syntax tree (`compiler/src/session/scopes.rs`); a `var` is visible after its statement, so its own initializer is not captured. |
| `textDocument/completion` | ok | `completion_members_and_scope`, `completion_ranks_and_replaced_range`, `completion_snippets_and_insert_replace_with_capable_client`, `completion_auto_import_and_truncation`, `completion_on_fallible_weak_and_type_parameter_receivers`, `member_completion_on_typed_expressions` | `textEdit` with the typed prefix (`InsertReplaceEdit` with `insertReplaceSupport`); server-side filter (case-insensitive subsequence) and `sortText` (prefix matches first, then locals > params > fields > members > own namespace > imported/std/builtins > auto-import > keywords; members in declaration order, fields first); cut to 200 with `isIncomplete`; call and statement snippets only with `snippetSupport`; keywords filtered by position; auto-import items carry the `import` line as `additionalTextEdits`. Members after `.` come from the receiver's sema type (any expression; `T!` and `weak *T` give `T`'s members, a type parameter its constraint interface's methods); namespaces and types by name fall back to the chain of names in the text. |
| `completionItem/resolve` | ok | `completion_resolve_adds_documentation` | — |
| `textDocument/signatureHelp` | ok | `signature_help_shows_parameters_and_doc` | Parameter label offsets follow the negotiated encoding. |
| `textDocument/inlayHint`, `inlayHint/resolve` | ok | `inlay_hints_types_and_parameter_names`, `inlay_hint_and_code_action_resolve` | — |
| `textDocument/codeAction`, `codeAction/resolve` | ok | `code_actions_fix_structured_diagnostics`, `inlay_hint_and_code_action_resolve`, `inspect::*`, `inline::*`, `signature::*` | Quick fixes (errors and lints `L0001`–`L0010`), extract variable/function, inline variable, organize imports, fill struct fields, implement interface (quick fix for `implements`/"does not implement", rewrite for other visible interfaces), `?` <-> `try ... catch err { return err }`; refactorings resolve lazily with `resolveSupport`; generated code is formatted; inline function (a call or every call) and change signature (add/remove/move parameter, across the workspace and an interface's implementations) — refused ones are listed `disabled` with the reason (`disabledSupport`). |
| `textDocument/codeLens`, `codeLens/resolve` | ok | `code_lenses_for_tests_main_and_references` | — |
| `textDocument/semanticTokens/full` | ok | `semantic_tokens_are_classified_and_stable` | — |
| `semanticTokens/full/delta` | ok | `semantic_tokens_delta_and_range` (edits applied client-side equal a fresh `full`) | — |
| `semanticTokens/range` | ok | `semantic_tokens_delta_and_range` (equals `full` restricted to the range) | — |
| Semantic token legend | ok | `features.rs` `SEMANTIC_TOKEN_TYPES` / `_MODIFIERS`; only these indices are emitted | — |
| `textDocument/foldingRange` | ok | `folding_ranges_cover_blocks_and_comments` | — |
| `textDocument/selectionRange` | ok | `selection_ranges_expand_from_the_token_to_the_file` | — |
| `textDocument/formatting`, `rangeFormatting` | ok | `formatting_whole_document_and_range` | — |
| `textDocument/onTypeFormatting` | ok | `on_type_formatting_formats_the_enclosing_block_only` | `}` formats the block it closes, a newline the innermost block around the cursor (except the cursor's line); only edits inside that block; nothing on a syntax error or at the top level. |
| `textDocument/prepareCallHierarchy`, `callHierarchy/incomingCalls`, `outgoingCalls` | ok | `call_hierarchy_incoming_and_outgoing` | A call is a reference followed by `(`; function values passed around are not calls. |
| `textDocument/prepareTypeHierarchy`, `typeHierarchy/supertypes`, `typeHierarchy/subtypes` | ok | `type_hierarchy_supertypes_and_subtypes` (embedding, a generic struct, `io.Reader` from std, a namespace that does not import the interface, an edit of the interface) | `typeHierarchyProvider` is added to the `initialize` JSON (lsp-types 0.95 has no field for it). Supertypes of a struct or enum: interfaces it satisfies and structs it embeds; of an interface: interfaces whose methods it has. A generic struct is shown at its declaration and satisfies an interface when some type arguments make it (`Get() T` satisfies `Get() int`); generic std structs are not in the table. Empty interfaces (and defaults-only ones) and the builtin `error` are left out. |
| `textDocument/documentLink` | ok | `imports_link_to_their_namespace` | `import a.b` (and `a.b.T`) links to the file declaring the namespace first (std: the first source declaring one of its members); a namespace spans files and directories, and editors do not open directories. Paths in `zell.toml` are not linked. |
| `textDocument/linkedEditingRange` | n.a. | — | No paired tags in Zell. |
| `textDocument/moniker` | n.a. | — | No cross-repository index format. |
| `textDocument/inlineValue` | missing | — | Debugger integration goes through the DAP agent. |
| `textDocument/documentColor`, `colorPresentation` | n.a. | — | — |

## Diagnostics

| Method / rule | Status | Evidence | Gap |
|---|---|---|---|
| `textDocument/publishDiagnostics` (push) | ok | `error_is_reported_with_range_and_cleared`, `diagnostics_carry_version_fix_data_and_cross_file_notes` | `version` of open documents. |
| `textDocument/diagnostic` (pull), push XOR pull | ok | `pull_diagnostics_for_documents_and_the_workspace` | — |
| `workspace/diagnostic` | ok | `pull_diagnostics_for_documents_and_the_workspace`, `workspace_diagnostics_stream_partial_results` | With `partialResultToken` one `$/progress` per file, then an empty result; unchanged reports are parked until an analysis changes something. |
| `workspace/diagnostic/refresh` | ok | `pull_diagnostics_for_documents_and_the_workspace` | — |
| Diagnostics cleared on `didClose` of an unsaved new file | ok | `closed_unsaved_documents_drop_their_diagnostics_and_answer_null` | — |
| `relatedInformation` | ok | `diagnostics_carry_version_fix_data_and_cross_file_notes` | Notes with a span: same file; the diagnostic's `related` notes (e.g. a duplicate declared in another file of the namespace): that file; notes without a span are appended to the message. |
| `tags` (`Unnecessary`) | ok | `unused_code_is_tagged_unnecessary` | `Deprecated` n.a. (no deprecation attribute yet). |
| `data` for quick fixes | ok | `diagnostics_carry_version_fix_data_and_cross_file_notes`, `code_actions_fix_structured_diagnostics` | `data` = the fixes with their edits and the document version; `codeAction` uses the context diagnostics' `data` at the current version, else recomputes from the analysis (clients that drop `data`, stale versions). |
| `codeDescription` | missing | — | No online error index to link to. |

## Workspace

| Method / rule | Status | Evidence | Gap |
|---|---|---|---|
| `workspace/symbol` (substring + fuzzy subsequence) | ok | `workspace_symbols_and_highlights` | — |
| `workspaceSymbol/resolve` | n.a. | — | Locations are always sent. |
| `workspace/didChangeWatchedFiles` (file edited outside the editor) | ok | `watched_files_are_added_and_removed` | Registered by the server (see dynamic registration above); a changed `zell.lock` or manifest of a root reloads its dependency sources. |
| `workspace/didChangeConfiguration` | ok | `inlay_hints_types_and_parameter_names`, `configuration_is_pulled_from_the_client` | `settings.zell` is applied, then the section is pulled when supported. |
| `workspace/configuration` (pull) | ok | `configuration_is_pulled_from_the_client` | Section `zell` after `initialized` and on each `didChangeConfiguration`; `initializationOptions` stay the fallback. |
| `workspace/didChangeWorkspaceFolders` | ok | `workspace_folders_are_added_and_removed` | `workspaceFolders.changeNotifications`; added folders are loaded as roots, removed ones unloaded (open documents stay). |
| `workspace/willRenameFiles` (edits imports and the namespace line) | ok | `renaming_a_file_updates_its_namespace_and_imports`, `renaming_a_file_of_a_shared_namespace_adds_imports` | — |
| `workspace/didRenameFiles` | ok | same tests | — |
| `workspace/willCreateFiles` etc., `didCreateFiles`, `didDeleteFiles` | missing | — | Covered by watched files. |
| `workspace/executeCommand` | ok | `signature::change_signature_command` | `zell.changeSignature` (`[{uri, position, params}]` → the `WorkspaceEdit`, refusals are errors); code lens commands run in the client. |
| `workspace/semanticTokens/refresh`, `inlayHint/refresh`, `codeLens/refresh` | ok | `refresh_requests_follow_changes_seen_by_other_documents`, `configuration_is_pulled_from_the_client` | Sent when another open document may show something different: tokens and hints when declarations (kind, name, signature) changed, lenses also when reference counts did; at most one of each per analysis; hints also after their settings change. |

## Robustness

| Check | Status | Evidence | Gap |
|---|---|---|---|
| 20k-line file: analysis + full semantic tokens < 1 s | ok | `robustness_large_files_edit_storms_and_broken_syntax` (~0.1 s for tokens in a debug build) | — |
| 200 rapid `didChange`: no backlog (`zell/analyzerStatus`) | ok | same test (< 20 analyses started, 0 queued) | — |
| Incremental analysis: an edit re-checks only the namespaces it reaches, results equal a full analysis | ok | `compiler/tests/incremental.rs` (random edits over `examples/`, `tests/spec/`, a generated project; `scripted_edits_equal_full_analysis`), `session::tests::edits_reanalyse_only_the_units_they_reach` | Per-namespace units keyed by text + dependency export signatures; whole-program checks (lints and unused imports only without errors, `main`) at assembly; see docs/TOOLING.md "Performance". |
| 232k-line workspace (50 namespaces): body edit < 100 ms to diagnostics | ok | `lsp/tests/server/bench.rs`, `compiler/tests/incremental_bench.rs` (ignored; release) | ~70 ms (was ~158 ms); a signature edit with 25 dependent namespaces ~270 ms (was ~620 ms). |
| Syntax error at every nesting level: no panic, positioned diagnostics, queries answer | ok | same test, `queries_work_in_a_file_with_syntax_errors` | — |
| Every example under every query | ok | `every_example_survives_all_queries` | — |
| A long request does not block others (worker pool over an `Arc` snapshot) | ok | `a_long_request_does_not_block_a_hover`; the server suite in `--release` with 32 test threads | — |

## Still missing for a gopls / rust-analyzer level server

Ranked by value for the cost (S < 1 day, M 1–3 days, L > 3 days):

1. ~~**Completion quality**~~ — done 2026-10-29 (`lsp/src/completion.rs`):
   `textEdit`/`InsertReplaceEdit`, `sortText` by relevance, truncation
   with `isIncomplete`, call/statement snippets with `snippetSupport`,
   auto-import items. Left: snippets for generic type arguments,
   postfix completions, ranking by expected type.
2. ~~**Incremental analysis**~~ — done 2026-10-29 (`compiler/src/session/`):
   per-namespace units; an edit re-parses its file, a body edit re-checks
   its namespace, a declaration change also its reverse dependencies, in
   one run with their dependencies as declarations only; std is indexed
   once per process. Numbers in docs/TOOLING.md "Performance". Left:
   assembling the `Analysis` is linear in the project (~30 ms at 232k
   lines: every unit's symbols and references are copied into one index),
   and a unit run re-registers the declarations of its whole dependency
   closure (~35 ms for 49 namespaces); caching the declaration phase per
   namespace, or a sharded index, would remove both.
3. ~~**Handlers off the main thread**~~ — done 2026-10-29
   (`lsp/src/snapshot.rs`): analysis requests run on a 2–4 thread pool
   against an `Arc` snapshot, cancellable while running, `ContentModified`
   on edits; the split is in docs/TOOLING.md. Left: answering position-free
   requests (workspace symbols) from the previous analysis while a new
   one runs, which needs both analyses in memory.
4. ~~**Dynamic registration + workspace folders + `workspace/configuration`**~~
   — done 2026-10-29 (`lsp/src/workspace.rs`).
5. ~~**Refresh requests**~~ — done 2026-10-29 (`lsp/src/workspace.rs`).
6. ~~**Type hierarchy**~~ — done 2026-10-29: method-set shapes per unit
   (`compiler/src/sema/shapes.rs`), satisfaction over the whole program
   at assembly (`compiler/src/session/typegraph.rs`), shared with
   `implementation`. Left: generic std structs, the builtin `error`.
7. ~~**Rename scoping**~~ — done 2026-10-29 (`compiler/src/session/scopes.rs`,
   `lsp/src/scoping.rs`); also type aliases are symbols now (definition,
   hover, references, rename).
8. ~~**Refactoring code actions**~~ — done 2026-10-29: extract
   variable/function, inline variable, organize imports
   (`lsp/src/refactor.rs`); fill struct fields, implement interface, `?`
   <-> `try ... catch err { return err }` (`generate.rs`, `errconv.rs`);
   inline function and change signature (`inline_fn.rs`, `signature.rs`,
   `zell.changeSignature`). Left: generic functions are not in the symbol
   index, so neither is offered for them.
9. ~~**`onTypeFormatting`, `documentLink`**~~ — done 2026-10-29
   (`assist.rs` `on_type_edits`, `links.rs`).
10. ~~**Diagnostics details**~~ — done 2026-10-29: `version`, `data` with
    the fixes, cross-file related information, partial results. Left:
    `codeDescription` (no online error index), `willSaveWaitUntil` (see
    above).
