#!/usr/bin/env python3
"""Quantize a fetched f32 embedding model to f16 (half-precision) in place.

Halves the npm download (~448 MB -> ~224 MB) with no practical retrieval-quality
loss: the Rust binary casts the f16 weights back to f32 on load (candle's
`VarBuilder::from_tensors` ends in `.to_dtype(F32)`), so compute precision is
unchanged — only the on-disk / download size shrinks, which also keeps the
package under npm's size limit.

Usage: quantize-model.py <src.safetensors> <dst.safetensors>
"""
import sys

from safetensors.numpy import load_file, save_file
import numpy as np


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit("usage: quantize-model.py <src.safetensors> <dst.safetensors>")
    src, dst = sys.argv[1], sys.argv[2]
    tensors = load_file(src)
    out = {
        k: (v.astype(np.float16) if v.dtype == np.float32 else v)
        for k, v in tensors.items()
    }
    save_file(out, dst)
    print(f"quantized {src} -> {dst} (f16)")


if __name__ == "__main__":
    main()
