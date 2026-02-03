"""Builds crates/whereti-extract/data/clip-labels.bin (PLAN.md §55): one
vector per label of tools/clip/labels.tsv, from CLIP ViT-B/32's text model,
so whereti only needs the image half of the model at run time.

Needs (pip): onnxruntime, tokenizers, numpy; and, in one folder, from
https://huggingface.co/Xenova/clip-vit-base-patch32 (revision in MODEL_REV):
  onnx/text_model.onnx, tokenizer.json

  python tools/clip/build_labels.py <that folder>

Output: "WCLP" + u32 labels + u32 dims, then labels x dims f32 (little
endian), each vector normalised, in the order of labels.tsv.
"""

import os
import struct
import sys

import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer

# Several phrasings per label, averaged: steadier than one (CLIP paper, §3.1.4).
TEMPLATES = ["a photo of a {}.", "a picture of a {}.", "a close-up photo of a {}."]


def labels(here):
    out = []
    with open(os.path.join(here, "labels.tsv"), encoding="utf-8") as f:
        for line in f:
            if line.startswith("#") or not line.strip():
                continue
            cols = line.rstrip("\n").split("\t")
            assert len(cols) == 8, line
            out.append(cols[0])
    return out


def main(src):
    here = os.path.dirname(os.path.abspath(__file__))
    names = labels(here)
    tok = Tokenizer.from_file(os.path.join(src, "tokenizer.json"))
    sess = ort.InferenceSession(os.path.join(src, "text_model.onnx"))
    outs = [o.name for o in sess.get_outputs()]
    vectors = []
    for name in names:
        acc = None
        for t in TEMPLATES:
            ids = tok.encode(t.format(name)).ids[:77]
            ids = np.array([ids + [ids[-1]] * (77 - len(ids))], dtype=np.int64)
            e = sess.run(None, {"input_ids": ids})[outs.index("text_embeds")][0]
            e = e / np.linalg.norm(e)
            acc = e if acc is None else acc + e
        vectors.append(acc / np.linalg.norm(acc))
    m = np.stack(vectors).astype("<f4")
    out = os.path.join(here, "..", "..", "crates", "whereti-extract", "data", "clip-labels.bin")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "wb") as f:
        f.write(b"WCLP" + struct.pack("<II", *m.shape))
        f.write(m.tobytes())
    print(f"{m.shape[0]} labels x {m.shape[1]} -> {out}")


if __name__ == "__main__":
    main(sys.argv[1])
