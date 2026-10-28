//! The sung words in Latin letters, for a reader who cannot sound out the script they are in.
//!
//! **Three engines, and which one answers is decided per run rather than per crate.** `uroman`
//! reads about a hundred scripts and is the general case. The two exceptions are the two scripts
//! this feature exists for, and both were measured against the spelling the lyric sites publish
//! before being written:
//!
//! - **Hangul is ours**, because uroman spells ㅊ `c` and the tense consonants `gg`/`dd`/`bb`, so
//!   꽃 came back `ggoc` against Revised Romanization's `kkot`. Ten of eighteen common words were
//!   wrong. RR is arithmetic on the syllable block and needs no dictionary, so there is nothing to
//!   trade for it.
//! - **Japanese is `kakasi`**, because uroman carries no Japanese readings at all and romanizes
//!   kanji as Mandarin: 世界 comes back `shijie` where the sheet wants `sekai`. Kana is the only
//!   thing that tells kanji from hanzi, and a line can carry none while its sheet does, which is
//!   why [`apply`] takes the whole sheet and settles the question once for all of it.
//!   Outside a Japanese sheet, Han stays as written: Chinese gets no automatic pinyin.
//!
//! Existing readings belong to the source and are kept for the whole sheet. Automatic readings
//! are generated on a blocking thread only while their display is enabled, then kept per line.

mod hangul;
mod japanese;

use hangul::{is_hangul_syllable, push_hangul};
use kakasi::IsJapanese;
use netodia_core::entities::lyrics::LyricLine;
use unicode_script::{Script, UnicodeScript};
use uroman::{Uroman, rom_format};

/// Cyrillic letters drawn like ASCII ones, both cases.
///
/// Uploaded sheets sometimes type English with these, and the line then carries a Cyrillic script
/// for [`needs_romanization`] to find. Without the guard a reader gets a nonsense second line
/// under a lyric they could already read.
const HOMOGLYPHS: [char; 32] = [
    'а', 'в', 'е', 'к', 'м', 'н', 'о', 'р', 'с', 'т', 'у', 'х', 'і', 'ј', 'ѕ', 'ԁ', 'А', 'В', 'Е',
    'К', 'М', 'Н', 'О', 'Р', 'С', 'Т', 'У', 'Х', 'І', 'Ј', 'Ѕ', 'Ԁ',
];

/// Whether the sheet lacks supplied readings and has a script we should sound out.
pub(super) fn needs_generation(lines: &[LyricLine]) -> bool {
    !lines
        .iter()
        .any(|line| line.romanization.as_deref().is_some_and(|reading| !reading.trim().is_empty()))
        && lines
            .iter()
            .any(|line| needs_romanization(&line.text, false) && !is_latin_in_disguise(&line.text))
}

/// Fill in automatic readings only when the sheet has none of its own.
///
/// Takes the sheet rather than a line because whether its Han characters are Japanese is a
/// question about the sheet: kana is the only evidence, and the line carrying it need not be the
/// line being romanized.
pub(super) fn apply(lines: &mut [LyricLine]) {
    if !needs_generation(lines) {
        return;
    }

    let japanese = lines.iter().any(|line| kakasi::is_japanese(&line.text) == IsJapanese::True);
    for line in lines {
        line.romanization = romanize_line(&line.text, japanese);
    }
}

/// The romanization to draw under `line`, or `None` where there is nothing to add.
///
/// The equality at the end is the second net under [`needs_romanization`]: a line an engine hands
/// back unchanged has nothing to say, and drawing it twice is worse than not drawing it.
fn romanize_line(line: &str, sheet_is_japanese: bool) -> Option<String> {
    if !needs_romanization(line, sheet_is_japanese) || is_latin_in_disguise(line) {
        return None;
    }

    let romanized = if sheet_is_japanese { japanese::romanize(line) } else { other_scripts(line) };
    let romanized = romanized.trim();
    (!romanized.is_empty() && romanized != line.trim()).then(|| romanized.to_owned())
}

/// Whether the line is written in a script worth sounding out.
///
/// The Unicode `Script` property rather than a range table of our own, which is the copy that goes
/// stale each Unicode release. `Common` and `Inherited` are punctuation, digits and combining
/// marks, and `Unknown` is a codepoint nothing can read, so an accented Latin line answers `false`
/// and grows no pointless `Como` under its `Cómo`.
///
/// Han only needs a reading when the sheet provides evidence that it is Japanese.
fn needs_romanization(line: &str, sheet_is_japanese: bool) -> bool {
    !line.is_ascii()
        && line.chars().any(|letter| match letter.script() {
            Script::Latin | Script::Common | Script::Inherited | Script::Unknown => false,
            Script::Han => sheet_is_japanese,
            _ => true,
        })
}

/// Whether the line is ASCII text typed with lookalike letters rather than a script of its own.
///
/// Both halves are load-bearing: every non-ASCII letter has to be a lookalike, and a real ASCII
/// letter has to sit beside them, so a genuinely Russian line is untouched.
fn is_latin_in_disguise(line: &str) -> bool {
    let mut ascii = false;
    let mut disguised = false;
    for letter in line.chars().filter(|letter| letter.is_alphabetic()) {
        if letter.is_ascii_alphabetic() {
            ascii = true;
        } else if HOMOGLYPHS.contains(&letter) {
            disguised = true;
        } else {
            return false;
        }
    }
    ascii && disguised
}

/// Everything that is not a Japanese sheet: our Hangul, Han as written, and uroman for the rest.
///
/// Split into runs rather than handed over whole because Hangul is the one script here with a
/// better answer than uroman's. Splitting costs uroman no context: what separates two Hangul runs
/// is spaces, punctuation and the occasional Latin word, none of which reads differently for
/// having a syllable beside it.
fn other_scripts(line: &str) -> String {
    let mut out = String::with_capacity(line.len() * 2);
    let mut rest = line;

    while !rest.is_empty() {
        let hangul_at = rest.find(is_hangul_syllable);
        let (before, from_hangul) = rest.split_at(hangul_at.unwrap_or(rest.len()));
        if !before.is_empty() {
            push_between_hangul(before, &mut out);
        }
        if from_hangul.is_empty() {
            break;
        }
        let run_len =
            from_hangul.find(|letter| !is_hangul_syllable(letter)).unwrap_or(from_hangul.len());
        let (run, after) = from_hangul.split_at(run_len);
        push_hangul(run, &mut out);
        rest = after;
    }
    out
}

/// What sits between two Hangul runs, romanized only if there is anything to romanize.
///
/// **The gaps between Korean words are runs**, so without the ASCII bail a Korean sheet walks
/// uroman's lattice once per space and pages a multi-megabyte archive in to be told that a space
/// is a space. Latin romanizes to itself, so skipping is the same answer for less: a sheet with
/// nothing but Hangul and English in it never touches uroman at all, and its dictionary stays
/// where it was compiled.
fn push_between_hangul(run: &str, out: &mut String) {
    if run.is_ascii() {
        out.push_str(run);
        return;
    }
    push_uroman(run, out);
}

/// uroman over one run, leaving Chinese spans as written even in mixed-script lyrics.
///
/// Deliberately no language hint. uroman takes an ISO 639-3 code and carries rules for twenty-odd
/// of them, but nothing upstream of here knows what language a sheet is in, and a wrong hint is
/// worse than none.
fn push_uroman(run: &str, out: &mut String) {
    let mut rest = run;
    while !rest.is_empty() {
        let han_at = rest.find(|letter: char| letter.script() == Script::Han);
        let (before, from_han) = rest.split_at(han_at.unwrap_or(rest.len()));
        if needs_romanization(before, false) {
            out.push_str(
                &Uroman::new().romanize_string::<rom_format::Str>(before, None).to_string(),
            );
        } else {
            out.push_str(before);
        }
        if from_han.is_empty() {
            break;
        }
        let run_len =
            from_han.find(|letter: char| letter.script() != Script::Han).unwrap_or(from_han.len());
        let (han, after) = from_han.split_at(run_len);
        out.push_str(han);
        rest = after;
    }
}

/// Whether the character at `at` is Han.
///
/// A position past either end is not, which is what lets a caller step off the front with
/// `wrapping_sub` and read "nothing there" rather than wrapping onto the last character.
fn is_han_at(letters: &[char], at: usize) -> bool {
    letters.get(at).is_some_and(|letter| letter.script() == Script::Han)
}

#[cfg(test)]
#[path = "tests/romanize_tests.rs"]
mod tests;
