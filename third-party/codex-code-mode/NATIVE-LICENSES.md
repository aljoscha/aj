# Native V8 redistribution evidence

**The external dependency notice gaps are closed, but binary release is not cleared.**
Do not treat the Rust crate's `MIT` metadata or Codex's `Apache-2.0` license as
covering the statically linked native archive. Shipping an AJ binary requires
the packaging and final-link checks below. This is bounded redistribution
evidence, not a reproducible-build attestation or legal opinion.

## Provenance

This investigation is specific to Codex commit
`44984d20817fc58026a8bbccf053e288c6897c8f` and its four Darwin/GNU Linux
pointer-compression/sandbox archive and binding pairs for Rusty V8 150.4.0.
`licenses/provenance.json` records their URLs and SHA-256 hashes from
`MODULE.bazel`, the evidence-file hashes, and each collected file's origin.
The native bootstrap verifies these artifact hashes, not license completeness.

The pinned `third_party/v8/README.md`, `BUILD.bazel`, and
`.github/workflows/rusty-v8-release.yml` describe Codex's Bazel-produced native
archives. These are not simply the binaries from denoland/rusty_v8's releases.
The graph incorporates ICU and custom libc++/libc++abi runtime objects.
The native bytes are identified by the recorded digests, but this investigation
does not independently reproduce their builds or attest their complete contents.

Verified sources:

- V8 15.0.245.2 source archive, SHA-256
  `164f3d3e0a8b4c02a386d3a88d1b1133d37e5c20be0239fecdcdde414a4340d5`,
  matching the integrity value in pinned `MODULE.bazel`. All root and non-test
  subtree `LICENSE*` files present in that archive are copied under `licenses/v8/`.
  This does not include external repositories fetched by its build graph.
- The crates.io `v8-150.4.0.crate`, SHA-256
  `42a978ff11f15b24e5c05a7123cf2b68f41e763546699781a924ef4e2cf43a49`,
  also matches `MODULE.bazel`. Its `.cargo_vcs_info.json` identifies Rusty V8
  commit `5c15a6995c9bb4bacd3e341b59fff32c909c80bf`. The MIT license was
  retrieved from that exact commit, not a moving branch.
- libc++, libc++abi, and llvm-libc license texts were retrieved from the exact
  Chromium Git revisions pinned in `MODULE.bazel`. ICU's full license and
  third-party notice text comes from the revision in `patches/v8_module_deps.patch`.
  Their immutable retrieval URLs and decoded-byte hashes are in the manifest.

## External native dependencies

`patches/v8_module_deps.patch` supplies the external repositories and
`patches/v8_bazel_rules.patch` connects them to V8's libraries. The four
Darwin/GNU Linux source archive targets in `third_party/v8/BUILD.bazel` depend
on `v8_150_4_0_binding` (which uses `@v8//:core_lib_icu`) and the custom C++
runtime. The workflow's matching pointer-compression/sandbox matrix uses these
Bazel targets. Header-only dependencies count here, even without named archive
members.

| Component | Resolved source | Collected files under `licenses/` |
| --- | --- | --- |
| Abseil | 20250814.1 | `abseil/LICENSE`, `AUTHORS` |
| Highway | 1.2.0 | `highway/LICENSE`, `LICENSE-BSD3` |
| simdutf | 7.7.0 | `simdutf/LICENSE-APACHE`, `LICENSE-MIT`, `AUTHORS`, `isadetection-NOTICE` |
| fast_float | 8.0.2 | `fast_float/LICENSE-APACHE`, `LICENSE-MIT`, `LICENSE-BOOST`, `AUTHORS` |
| Dragonbox | `beeeef91cf6fef89a4d4ba5e95d47ca64ccb3a44` | `dragonbox/LICENSE-Apache2-LLVM`, `LICENSE-Boost` |
| FP16 | `3d2de1816307bac63c16a297e8c4dc501b4076df` | `fp16/LICENSE` |

Abseil's requested V8 module version is 20250814.0, but Codex's lockfile selects
20250814.1's `source.json`. The collected registry metadata matches the
lockfile's SHA-256 values, and the downloaded source matches that metadata's
integrity. The root override patches Windows thread identity without pinning
an older version. Highway, simdutf, and fast_float archive bytes match the
SHA-256 values in the dependency patch. Dragonbox and FP16 notices were checked
against the exact Chromium Git revisions, not V8 wrapper licenses. Every
collected byte hash and source URL is in `licenses/provenance.json`.

The root license/author files were collected from all six sources. No separate
NOTICE file was present in these source archives. simdutf's compiled header
`include/simdutf/internal/isadetection.h` contains an additional BSD-3-Clause
notice that requires binary attribution. `isadetection-NOTICE` preserves its
entire opening comment, lines 1–44, byte-for-byte. Its full-source hash and
extraction range are recorded. Benchmark-only simdutf competitor licenses and
Dragonbox's unreferenced `subproject/3rdparty` are not native graph inputs and
are not copied. A full source-archive redistribution must preserve those too.

## Established notice requirements

- V8's BSD terms require copyright, conditions, and disclaimer in binary
  distribution documentation or other materials. Preserve both `LICENSE` and
  `LICENSE.v8`, along with applicable Strongtalk and fdlibm notices.
- Rusty V8's MIT text requires its copyright and permission notice with copies
  or substantial portions. Include `licenses/rusty-v8/LICENSE`.
- LLVM component texts include Apache-2.0 with LLVM exceptions and legacy terms.
  Preserve their complete texts rather than substituting a generic Apache license
  or assuming the exceptions eliminate every distribution obligation.
- ICU's Unicode and embedded third-party notices must be considered together.
  Include its full text, not just the leading Unicode license.
- V8's other subdirectory licenses have their own terms. The collected set is
  deliberately broader than a proven list of linked components. Presence here
  does not establish inclusion in a particular native binary.
- Abseil is Apache-2.0. Highway offers Apache-2.0/BSD-3-Clause, simdutf offers
  Apache-2.0/MIT, fast_float offers Apache-2.0/MIT/Boost, and Dragonbox offers
  Apache-2.0 with LLVM exceptions/Boost. The alternative texts are retained,
  not combined into a new license. FP16 is MIT. Include the simdutf embedded
  BSD notice regardless of the root license choice.

Codex's extracted `LICENSE`, `NOTICE`, and modification notices remain necessary.
The notice files here are unmodified upstream texts. `licenses/evidence/`
contains registry/build evidence, not a claim that every toolchain source is
linked. This document summarizes evidence and does not replace license terms.

## glibc: trig fork versus system runtime

The local `native/archive.a.gz` matches the x86_64 GNU Linux artifact digest
`a35c75d1f26e6a983885a45b33490a4ebe54f05050568b32b89cfb421b30b583`.
Its decompressed archive digest is
`82e3368ef77a5427d781104c3903fcffadfaef1fc324d55162cfa598a509e927`.
`ar t` reports 1,810 members. GNU `nm -A -C` reads it without diagnostics.
Commands, tool versions, and representative observations are in the manifest.

The pinned V8 source gives a useful discriminator, not just missing names:
`src/base/ieee754.h` and `.cc` select `sin`/`cos` when
`V8_USE_LIBM_TRIG_FUNCTIONS` is absent. The other branch selects
`fdlibm_sin`/`fdlibm_cos` plus `glibc_sin`/`glibc_cos`. The archive defines
`v8::base::ieee754::sin(double)` and `cos(double)` in `ieee754.pic.o`, with no
`glibc_sin`, `glibc_cos`, `__branred`, `fdlibm_sin`, or `fdlibm_cos` symbols.
There are no `branred`, `s_sin`, or `sincostab` members. It also defines
Abseil `lts_20250814`, Highway, and simdutf symbols, consistent with the graph.

V8's `BUILD.gn` lists the glibc trig sources behind
`v8_use_libm_trig_functions`. Its `BUILD.bazel` and the Codex Bazel patches do
not select those sources or define that macro. Together, the source selection
and positive fdlibm-path symbols are proportionate evidence that **the V8
glibc trig fork is not incorporated in the inspected archive**. Its generic
`README.v8` “shipped” marker is not a target-specific finding. The LGPL text
remains collected conservatively. The other three archives have the same
Bazel source selection, but their members/symbols were not inspected here.

This does not mean “no glibc obligations.” The GNU custom libc++/libc++abi
targets explicitly use `gnu_libc_headers` from hermetic LLVM 0.8.11. Its
lockfile-verified source archive, glibc repository definitions, header URL/hash
index, and bundled glibc notices are recorded under `licenses/evidence/`.
That header target contains headers, not a static glibc library. The native
archive has undefined C library references such as `malloc` and `memcpy`, which do
not themselves show that glibc implementation objects were copied into it.
Final AJ linkage determines the system runtime actually distributed. Header
inlines/macros require separate consideration under LGPL-2.1 section 5,
including its small-inline/accessor exception. If non-exempt LGPL code is
incorporated, section 6's applicable notice, source and relinking requirements
must be satisfied. Keeping a license file alone does not satisfy that case.

## Remaining release work and limits

1. Include the applicable collected license/attribution texts and Codex notices
   in the actual binary package. For native source redistribution, retain source
   notices and the producer's patches/modification notices. A URL is provenance,
   not a substitute for delivering required notices or corresponding source.
2. Resolve the GNU toolchain-header case above and inspect the final binary's
   runtime linkage. If shipping glibc or incorporating non-exempt LGPL code,
   supply the applicable source/relinking mechanism and notices. If relying on
   the user's shared runtime, document that packaging choice. This review does
   not establish that final-link result or clear a statically linked AJ release.
3. Before shipping another target, check its actual archive/runtime selection
   against this graph, especially any glibc trig or additional runtime objects.
   No byte-for-byte rebuild or absolute source-closure proof is required for
   this notice review. The source pins, build selection, and observed symbols
   are evidence with the target limits stated above.
4. Repeat on upgrades or overridden native artifacts. Parent Rust dependencies
   and final AJ packaging are separate. The importer does not refresh or certify
   this collection.

One source-build discrepancy was observed without changing upstream: applying
`v8_bazel_rules.patch` with GNU patch to the hash-matched V8 tarball rejects its
first `bazel/defs.bzl` flag-helper hunk. The dependency/BUILD hunks apply, and
the rejected hunk does not select glibc sources. This investigation therefore
does not claim to have successfully reconstructed or built the producer tree.
