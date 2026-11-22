/*---
description: Intl.Segmenter grapheme, word, and sentence segmentation with containing()
---*/
const gSeg = new Intl.Segmenter("en", { granularity: "grapheme" });
const graphemes = Array.from(gSeg.segment("a👨‍👩‍👧‍👦b"));
assert.sameValue(graphemes.length, 3);
assert.sameValue(graphemes[1].segment, "👨‍👩‍👧‍👦");
assert.sameValue(graphemes[2].index, 12);
assert.sameValue("isWordLike" in graphemes[0], false);

const wSeg = new Intl.Segmenter("en", { granularity: "word" });
const segs = wSeg.segment("Hello, world!");
const words = Array.from(segs).filter((s) => s.isWordLike).map((s) => s.segment);
assert.sameValue(words.join("|"), "Hello|world");
assert.sameValue(segs.containing(8).segment, "world");
assert.sameValue(segs.containing(999), undefined);

const sSeg = new Intl.Segmenter("en", { granularity: "sentence" });
const sentences = Array.from(sSeg.segment("One. Two.")).map((s) => s.segment.trim());
assert.sameValue(sentences.length, 2);
assert.sameValue(sentences[0], "One.");
assert.sameValue(sentences[1], "Two.");
