# Agent prompts

Ready-to-paste prompts for long waves of work that need more CPU and disk
than a cloud session has. Run `claude` locally in the repository root and
paste the prompt from the file.

| File | Wave |
|---|---|
| `perf-wave-1.md` | the five hot spots of the 2026-10-22 benchmark run: scheduler, I/O, JSON, crypto, compiler |
| `perf-wave-3.md` | the plan after the 2026-10-23 wave: ten sized items (flaky `cycle_server`, timers, open-coded `defer`, sentinel errors, LTO, `log`, `data`, crypto pass 2, scheduler leftovers, I/O) plus the one-agent/short-test working rules |
| `linux-release.md` | publish v0.1.0 for Linux: green Linux CI and `bundle`, the self-contained archive verified from a clean container and from custom install dirs, release dry run with `hosts=linux`, tag and GitHub Release |
| `macos-ci.md` | macOS as a first-class host: green `ci` (incl. the lldb tests), the self-contained archive built on a Mac and tested on a clean Mac, ad-hoc signing, release dry run |
| `windows-ci.md` | Windows as a first-class host: green `ci`, the self-contained archive built and tested on Windows, release dry run; lists the open macOS items for a later Mac run |

Planned next (not written yet): logger and ORM hot spots
(RESULTS-framework.md), the self-contained distribution
(docs/PLATFORMS.md "Дистрибутив": bundled lld, glibc/musl/mingw/macOS link
inputs for every target), test-suite speed-up, moving `data` into a
package (docs/FRAMEWORK.md plan; `web` already left the tree, git tag
`web-in-tree`).
