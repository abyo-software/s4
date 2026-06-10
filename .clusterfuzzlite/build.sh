#!/bin/bash -eu
# ClusterFuzzLite build script (OSS-Fuzz 互換)。
#
# `cargo bolero build-clusterfuzz` が s4-codec の bolero #[test] ハーネス
# (tests/fuzz_bolero.rs) を libFuzzer バイナリ + per-target wrapper
# (`<test_name>_fuzzer`) に変換して target/fuzz/clusterfuzz.tar に固める。
# ClusterFuzz は `*_fuzzer` という名前の実行ファイルを fuzz target として
# 検出するので、tar を $OUT に展開するだけで良い。

cd "$SRC/s4"

# OSS-Fuzz の compile が export する RUSTFLAGS には `--cfg fuzzing` が単独で
# 入っており、bolero の engine 選択 cfg (`fuzzing_libfuzzer` 等が無いまま
# `fuzzing` だけ立つ) と衝突して target 列挙の `cargo test --no-run` が
# コンパイルエラーになる。cargo-bolero は必要な flag (--cfg fuzzing /
# --cfg fuzzing_libfuzzer / sancov / -Zsanitizer) を全部自前で組み立てて
# env の RUSTFLAGS を「追加で」継承するだけなので、ここで捨てて良い。
unset RUSTFLAGS RUSTDOCFLAGS

# Rust で ClusterFuzzLite がサポートする sanitizer は address のみ。
# それ以外 (coverage 等) が来たら sanitizer なしでビルドする。
case "${SANITIZER:-address}" in
address)
    BOLERO_SANITIZER="address"
    # OSS-Fuzz の CFLAGS (-fsanitize=address) で zstd-sys 等の C コードは
    # ASan 計装される。cargo-bolero の target 列挙ステップ (sanitizer なしの
    # `cargo test --no-run`) がそれをリンクすると ASan runtime 不在で
    # undefined reference になるため、runtime のリンクだけ env で常時有効化。
    export RUSTFLAGS="-Zsanitizer=address"
    # bolero-libfuzzer は vendored libFuzzer (C++) を CXXFLAGS で compile し
    # `stdc++` をリンクする。OSS-Fuzz の CXXFLAGS にある -stdlib=libc++ だと
    # final link (rustc) で libc++ シンボルが未解決になるので外す。
    export CXXFLAGS="${CXXFLAGS//-stdlib=libc++/}"
    ;;
*)
    BOLERO_SANITIZER="NONE"
    # sanitizer なしビルドに ASan 計装済み C objects が混ざらないようにする
    unset CFLAGS CXXFLAGS
    ;;
esac

cargo bolero build-clusterfuzz \
    --package s4-codec \
    --profile fuzz \
    --sanitizer "$BOLERO_SANITIZER"

tar -C "$OUT" -xf target/fuzz/clusterfuzz.tar

# Seed corpus: リポジトリに commit 済みの corpus
# (crates/s4-codec/tests/__fuzz__/<test>/corpus/) を
# `<target>_seed_corpus.zip` として各 wrapper の隣に置く。
for wrapper in "$OUT"/*/*_fuzzer; do
    [ -e "$wrapper" ] || continue
    test_name="$(basename "${wrapper%_fuzzer}")"
    corpus="crates/s4-codec/tests/__fuzz__/${test_name}/corpus"
    if [ -d "$corpus" ] && find "$corpus" -type f -print -quit | grep -q .; then
        zip -q -j "${wrapper}_seed_corpus.zip" "$corpus"/*
    fi
done
