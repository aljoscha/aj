# Native V8 redistribution evidence

**Native notice evidence is refreshed, but binary release is not cleared.**
Do not treat the Rust crate's `MIT` metadata or Codex's `Apache-2.0` license as
covering the statically linked native archive. Shipping an AJ binary requires
the packaging and final-link checks below. This is bounded redistribution
evidence, not a reproducible-build attestation or legal opinion.

## Provenance and scope

This review covers Codex commit `75fc67e8f4284a669eebd90ab6bc9ef7697e876e`,
Rusty V8 152.2.0 from the OpenAI fork, and the four Darwin/GNU Linux
pointer-compression/sandbox archive and binding pairs consumed by AJ.
`licenses/provenance.json` records their declared URLs and SHA-256 hashes from
`MODULE.bazel`, source/build evidence hashes, and each collected file's origin.
This licensing review did not download or inspect the new native artifacts.
The native bootstrap checks artifact hashes, not license completeness.
Windows and musl artifacts now present upstream are outside this review.

Verified source downloads:

- V8 15.2.124.1 archive, SHA-256
  `68d3791b8dbba906cfff9a3e35738d81747c66f4ba9a37fa99f008ea3b71cb32`,
  matches the `MODULE.bazel` integrity. All root and non-test subtree
  `LICENSE*` files are collected under `licenses/v8/`. The newly present
  disarm and fadec licenses are retained conservatively. Codex's engine importer
  says neither package is in its closure. V8's former `third_party/glibc`
  directory is absent, so its obsolete wrapper notices were removed.
- Rusty V8 fork archive at `90a65f887a0c0f048f3784afbcb320b373c8a2e0`,
  SHA-256 `b0b188d1f1d851abd78337818a28a15e84f143b082ce966976e9206617684373`,
  matches `MODULE.bazel`. Its MIT `LICENSE` is unchanged. The collected
  `Cargo.toml` identifies version 152.2.0. This replaces the former crates.io
  150.4.0 archive and its `.cargo_vcs_info.json`, not just its version label.
- libc++, libc++abi, llvm-libc, ICU, and FP16 notices were fetched from their
  exact Chromium Git revisions. Decoded-byte hashes and immutable URLs are
  recorded. libc++ and llvm-libc revisions changed, but their license texts
  did not. libc++abi remains at the previous revision.
- Highway, fast_float, and the hermetic LLVM override archives were downloaded
  and matched to the new graph's hashes. Unchanged Abseil, simdutf and Dragonbox
  notices retain their prior collection evidence. Their exact archive hashes
  or Git revisions were compared with the new declarations.

## Native source graph and dependencies

The old `v8_module_deps.patch` and `v8_bazel_rules.patch` are no longer the
source of this graph. `third_party/v8/engine_repository.bzl` imports filegroups
from V8's pinned BUILD, applies four patches, and replaces its MODULE/root BUILD
with Codex-owned policy. `engine.BUILD.bazel` supplies the engine targets.
`dependencies.bzl`, `abseil_repository.bzl`, and dependency BUILD overlays own
the external sources. These files, artifact profiles, native runtime rules,
patches, checksum manifests, and release workflow are hashed in the manifest.

The workflow selects `rusty_v8_sandbox_release_pair_<target>` for the reviewed
artifacts. `artifact_profile.bzl` selects pointer compression, sandboxing and
Chromium's custom libc++ runtime for these release profiles. The binding target
uses `@v8//:core_lib_icu`, and runtime objects are folded into the static archive.
The ordinary Bazel source profile instead uses the workspace C++ runtime on
Darwin/Linux. It must not be confused with the published release profile.
The pinned workflow describes the producer path, not a proof that the published
bytes were built from this checkout or a complete inventory of their contents.

| Component | Resolved source | Collected files under `licenses/` |
| --- | --- | --- |
| Abseil | 20250814.1 | `abseil/LICENSE`, `AUTHORS` |
| Highway | `2607d3b5b0113992fe84d3848859eae13b3b52c1` | `highway/LICENSE` |
| simdutf | 7.7.0 | `simdutf/LICENSE-APACHE`, `LICENSE-MIT`, `AUTHORS`, `isadetection-NOTICE` |
| fast_float | 8.2.10 | `fast_float/LICENSE-APACHE`, `LICENSE-MIT`, `LICENSE-BOOST`, `AUTHORS` |
| Dragonbox | `beeeef91cf6fef89a4d4ba5e95d47ca64ccb3a44` | `dragonbox/LICENSE-Apache2-LLVM`, `LICENSE-Boost` |
| FP16 | `782eea126dc5c755827be751a099eb01826175cf` | `fp16/LICENSE` |
| ICU | `d578f2e8b7bd5938e21cfb6bf15c079e0aa5b738` | `icu/LICENSE` |
| libc++ | `b16984ce99c702355a5b2b4c52574e82cec41fb9` | `libcxx/LICENSE.TXT` |
| libc++abi | `8f11bb1d4438d0239d0dfc1bd9456a9f31629dda` | `libcxxabi/LICENSE.TXT` |
| llvm-libc | `75ae50cd48c8a9d70f552ccff040029968778da9` | `libc/LICENSE.TXT` |

V8's private Abseil now uses a direct hash-pinned archive, a lifetime-capture
patch and the `codex_v8_lts_20250814` namespace. The retained Abseil registry
metadata is prior corroborating evidence, not the new V8 resolution mechanism.
Highway's single LICENSE now includes Apache, BSD and CC0 terms, replacing the
separate LICENSE-BSD3 file. ICU's full updated third-party notice text is copied.
Header-only dependencies count even without named archive members.

simdutf's compiled `include/simdutf/internal/isadetection.h` contains an additional
BSD-3-Clause notice. `isadetection-NOTICE` retains its opening comment, lines
1–44, byte-for-byte, with the source hash and range recorded. Its source pin
has not changed. Benchmark-only competitor licenses and Dragonbox's unreferenced
`subproject/3rdparty` are not native graph inputs. Full source redistribution
must preserve those too. This collection is not a full source distribution.

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
- Abseil is Apache-2.0. Highway offers Apache-2.0/BSD-3-Clause with a CC0 exception for
  `hwy/contrib/random/random-inl.h`, simdutf offers
  Apache-2.0/MIT, fast_float offers Apache-2.0/MIT/Boost, and Dragonbox offers
  Apache-2.0 with LLVM exceptions/Boost. The alternative texts are retained,
  not combined into a new license. FP16 is MIT. Include the simdutf embedded
  BSD notice regardless of the root license choice.

Codex's extracted `LICENSE`, `NOTICE`, and modification notices remain necessary.
The notice files here are unmodified upstream texts. `licenses/evidence/`
contains registry/build evidence, not a claim that every toolchain source is
linked. This document summarizes evidence and does not replace license terms.

## glibc and system runtime limitations

The previous x86_64 GNU Linux archive inspection belongs to Rusty V8 150.4.0.
It is retained only as `historical_native_inspection` in the manifest. Its
1,810-member count, fdlibm-path symbols and digests are **not findings about
152.2.0**. No new archive member/symbol inspection or final AJ link audit was
performed here.

The hash-verified V8 15.2.124.1 archive contains no `third_party/glibc` paths.
Its `BUILD.bazel`/`BUILD.gn` no longer contain the old glibc trig selection, and
its math sources have changed. The old `sin`/`cos` symbol discriminator therefore
must not be carried forward. This source observation does not establish that a
final binary contains no glibc code.

The GNU custom libc++/libc++abi graph still uses `gnu_libc_headers`. The root
module declares LLVM 0.8.11 but **overrides its source** with hermetic LLVM commit
`d27493f0386c9d4d28f7b12630cb91b0cdfd20f1`, archive SHA-256
`2938e58c5100f1d99790ea15cfa5afdf2aa431faab6d2e620effde7f8198b2ed`.
`licenses/evidence/hermetic-llvm/` now contains that archive's glibc repository
definitions, header URL/hash index and bundled glibc notices. The definitions
and index changed. The notices did not. The old LLVM registry source record was
removed because it does not describe the override actually selected.

That target provides headers, not a static glibc library. Header inlines/macros
require separate consideration under LGPL-2.1 section 5, including its
small-inline/accessor exception. If non-exempt LGPL code is incorporated,
section 6's applicable notice, source and relinking requirements must be met.
Keeping a license file alone does not satisfy that case. Final AJ linkage and
packaging determine the system runtime actually distributed.

## Verification and remaining release work

The refresh verified source-archive hashes against MODULE/dependency declarations,
fetched the changed Git-pinned notices, and checked every collected file against
the manifest. Unchanged notice bytes are not represented as changed terms merely
because their source revision changed. The full source license inventory was
reviewed for V8 and the downloaded Highway/fast_float archives.

A source reconstruction limitation remains: sequential GNU
`patch --batch --fuzz=0 -p3` on files extracted from the hash-matched V8 archive
applies the portability and array-sort patches (some hunks with offsets), but
rejects the first `bazel/defs.bzl` hunk in `v8_defs.patch`. The subsequent
`v8_windows_snapshot_wine.patch` also rejects its first hunk against that
partially patched file. This was repeated from clean scratch. No upstream file
was changed. This is not a Bazel `ctx.patch` test, successful engine import,
producer rebuild or reproducible-build attestation.

Before binary release:

1. Deliver the applicable license/attribution texts and Codex notices in the
   actual package. For source redistribution, preserve producer modifications,
   patch notices and source notices. URLs alone do not deliver required notices
   or corresponding source.
2. Resolve GNU header-code obligations and inspect final runtime linkage. If
   shipping glibc or incorporating non-exempt LGPL code, supply the applicable
   source/relinking mechanism and notices. If relying on the user's shared
   runtime, document that packaging choice. This review does not clear a
   statically linked AJ release.
3. Inspect the new target archive/runtime selection before shipping. This refresh
   supplies source-level notice evidence, not new native-content verification.
   Windows/musl require their own runtime and notice review. No byte-for-byte
   rebuild or absolute source-closure proof is claimed or required by this review.
4. Repeat for future upgrades or overridden artifacts. Parent Rust dependencies
   and final AJ packaging are separate. The importer does not refresh or certify
   this collection.
