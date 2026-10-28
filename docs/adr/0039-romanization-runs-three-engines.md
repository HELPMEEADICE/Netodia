# ADR 39: Romanization runs three engines, and the Korean one is ours

**Status:** Accepted, 2026-09-08

A lyrics panel that shows a Hangul, kana, Cyrillic or Arabic sheet exactly as written gives a reader
who cannot sound the script out a page of shapes. Singing along is the whole point of the panel, and
for a large share of the world's music it was doing nothing at all.

Decision: three engines behind one module. `uroman` handles the general case and covers about a
hundred scripts. Japanese goes to `kakasi`. Korean is romanized by a Revised Romanization pass
written here.

Alternatives: uroman on its own; kakasi in front of it for both CJK scripts; a dictionary-backed
engine per language; showing the sheet as written and leaving the reader to it.

Trade: one general engine is the right default, and it is wrong about exactly the two languages this
feature was built for. uroman doubles the tense consonants and reads the affricate as `c`, which is
not the romanization any Korean reader has met, and ten of eighteen common lyric words came back
wrong. A romanization that is wrong more often than not is worse than none, because the reader has
no way to tell which lines to distrust. For Japanese, uroman carries no Japanese readings at all and
falls back to Mandarin, kanji and hanzi being the same codepoints. Against that, three engines are
three things that can break, one more dependency, and one table maintained by hand. The Japanese
choice also has to be made for a whole sheet rather than line by line, because kana is the only
thing that separates kanji from hanzi and a line can carry no kana while its sheet does.

Revised Romanization is arithmetic on the syllable block, three tables and no dictionary, which is
what makes it small enough to own. A final consonant carries onto a following silent initial, an
aspirate elides in the move, a two-consonant final splits across the boundary, and a moved dental
palatalizes before the close front vowel. That reaches twenty-four of twenty-five. The one it misses
is assimilation across a syllable boundary, which needs to know where the words are, and knowing
that needs the dictionary this approach was chosen for not having.

The licence is the part that is expensive to undo. `kakasi` is GPL-3.0. An AGPL-3.0-or-later binary
may link it under section 13 of GPLv3, with the combined work staying AGPL, so this is permitted
rather than merely convenient. It is not the first copyleft dependency here: Slint is tri-licensed
and an AGPL project takes its GPL-3.0 arm, and Symphonia is MPL-2.0. It is the first whose only
offer is copyleft, with no permissive arm to fall back on, and that is what turned
[ADR 6](0006-agpl-and-what-artifacts-owe.md)'s obligation half into work. `licenses/` now carries
the GPL-3.0 text, in all five package formats, and `crates/netodia/tests/packaging.rs` is what keeps
it there.

The cost that shows up at runtime is memory rather than time. The general engine pages in a
multi-megabyte data file the first time it is called, so a sheet that is already Latin must never
reach it. Latin romanizes to itself, so bailing out before the call changes nothing in the output
and a great deal in what the process is holding. The pass runs once per track change on a blocking
thread, never on the thread that draws, and the display toggle filters rows that are already in hand
rather than resolving them again.

This ADR was written in September 2026 from the lyrics working doc, deleted when that feature
shipped.

**Amended, 2026-10-27:** Supplied readings take priority for the whole sheet. Only a sheet without
them is eligible for automatic readings, and generation now follows the reading-display toggle.
Enabling it supplements the resident sheet on the blocking pool without another lookup; disabling
it hides any readings already in hand. Chinese no longer receives automatically generated pinyin.
