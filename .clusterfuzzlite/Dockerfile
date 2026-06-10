# ClusterFuzzLite build image — OSS-Fuzz 互換の libFuzzer バイナリを作る。
# bolero ハーネス (crates/s4-codec/tests/fuzz_bolero.rs) を
# `cargo bolero build-clusterfuzz` で OSS-Fuzz 形式に変換する。
FROM gcr.io/oss-fuzz-base/base-builder-rust

# base image の pinned nightly (2025-09-05 = 1.91) は s4 の MSRV (1.92) より
# 古いので、新しめの nightly に date pin で切り替える。
RUN rustup toolchain install nightly-2026-06-01 --profile minimal
ENV RUSTUP_TOOLCHAIN=nightly-2026-06-01

# libfuzzer engine だけで良い (afl / kani の bin build を省いて image build を短縮)
RUN cargo install cargo-bolero --locked --no-default-features --features libfuzzer

COPY . $SRC/s4
COPY .clusterfuzzlite/build.sh $SRC/build.sh
WORKDIR $SRC/s4
