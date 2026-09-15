//! Source comments may name a milestone that is **done**, never one that is not.
//!
//! # Why this is a test and not a note
//!
//! When the maintainer re-planned the run after milestone 11, five numbers moved — and the
//! stale references were in *source comments* as much as in the docs. `resample.rs`
//! named "milestone 13", `grain.rs` named "12b" three times, `lib.rs`, `main.rs` and
//! `hotkeys.md` each named "12a". None of them was wrong when it was written and all
//! of them were wrong an hour later.
//!
//! The rule that falls out of it:
//!
//! > **A backward reference is archaeology and cannot rot. A forward reference is a
//! > forecast, and forecasts rot silently.**
//!
//! "Milestone 10 added a list of brush dabs" is true for ever. "12a resolves it" was
//! true for about six weeks, and nothing failed when it stopped being true — which is
//! the whole problem, because a comment that is confidently wrong is worse than no
//! comment. So a forward reference names **the thing**: "the colour transition",
//! "output sharpening", "chemical toning". That is what the comment meant anyway, and
//! it survives any amount of renumbering.
//!
//! `docs/status.md` is the one document whose job is to know current completion, and
//! `docs/milestones.md` records the order. The
//! frozen briefs are deliberately left quoting the old ones — they are a record of
//! what was decided when, and editing them would be falsification rather than a fix.
//! Neither is scanned here.

use std::path::{Path, PathBuf};

/// The milestones that are completely finished.
///
/// This must be an explicit set rather than “built through N”: 16 was deliberately
/// deferred while 17, 18 and 19 shipped. A high-water mark would silently bless
/// comments that name work which still does not exist.
const COMPLETED: &[u32] = &[
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 17, 18, 19,
];

fn is_complete(number: u32) -> bool {
    COMPLETED.contains(&number)
}

#[test]
fn no_source_comment_names_an_unbuilt_milestone() {
    let mut offences = Vec::new();
    for file in sources() {
        let text = std::fs::read_to_string(&file).expect("readable source");
        for (n, line) in text.lines().enumerate() {
            for number in milestone_numbers(line) {
                if !is_complete(number) {
                    offences.push(format!(
                        "{}:{}\n      {}",
                        file.display(),
                        n + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    assert!(
        offences.is_empty(),
        "source comments name milestones that are not complete.\n\
         Name the thing, not the number — \"the colour transition\", not \"12a\".\n\
         See this file's module note.\n\n  {}\n",
        offences.join("\n  ")
    );
}

#[test]
fn the_scanner_finds_the_references_it_is_meant_to() {
    // **Seen to work before it is trusted.** A scanner that matched nothing would
    // pass this suite silently for ever, which is the exact failure mode it exists to
    // prevent — so both forms it has to catch are exercised, and so are the shapes it
    // must *not* catch, because a false positive on `2x2` or `0.75` would make the
    // test something people switch off.
    assert_eq!(
        milestone_numbers("// Milestone 12 adds the thing"),
        vec![12]
    );
    assert_eq!(
        milestone_numbers("/// see milestone 9a for the reason"),
        vec![9]
    );
    assert_eq!(milestone_numbers("// 12b's toning consumes this"), vec![12]);
    assert_eq!(
        milestone_numbers("// 10c produced the clearest case"),
        vec![10]
    );
    assert_eq!(
        milestone_numbers("// milestone 3 and milestone 14"),
        vec![3, 14]
    );

    assert!(milestone_numbers("// only 2x2 Bayer is handled").is_empty());
    assert!(milestone_numbers("// a 0.75 fill ratio, 8192 wide").is_empty());
    assert!(milestone_numbers("// the D50 white point, sRGB, Rec2020").is_empty());
    assert!(milestone_numbers("// 1.5% of the diagonal").is_empty());
    assert!(milestone_numbers("// see AGX_PIVOT_X").is_empty());

    // And the guard itself: a forward reference in this very file's prose would be
    // caught, so the module note above deliberately quotes numbers only as history.
    //
    // The non-contiguous case is the reason this is a set: 18 and 19 are finished
    // while 16, between them, is not. A high-water-mark implementation would accept
    // all three and defeat the guard.
    assert!(is_complete(18));
    assert!(is_complete(19));
    assert!(!is_complete(16));
    assert_eq!(milestone_numbers("// milestone 16 resolves it"), vec![16]);
}

/// Milestone numbers named on one line, in either form the codebase has used:
/// `milestone 13`, and the bare sub-milestone `12b`.
///
/// The bare form is restricted to a digit-pair followed by a single `a`–`c`, which is
/// what a sub-milestone looks like and what nothing else in this codebase does. Bare
/// `13` on its own is deliberately **not** matched: it is indistinguishable from every
/// other number, and the false positives would be constant.
fn milestone_numbers(line: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let lower = line.to_ascii_lowercase();
    let bytes = lower.as_bytes();

    // `milestone <n>`
    let mut from = 0;
    while let Some(at) = lower[from..].find("milestone ") {
        let start = from + at + "milestone ".len();
        let digits: String = lower[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(n) = digits.parse::<u32>() {
            out.push(n);
        }
        from = start;
    }

    // Bare `12b`, bounded so it is a token rather than the tail of something.
    for (i, w) in lower.char_indices() {
        if !w.is_ascii_digit() {
            continue;
        }
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'.';
        if !before_ok {
            continue;
        }
        let rest = &lower[i..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let after = &rest[digits.len()..];
        let suffixed = after.starts_with(['a', 'b', 'c'])
            && !after[1..].starts_with(|c: char| c.is_ascii_alphanumeric());
        if suffixed
            && (1..=2).contains(&digits.len())
            && let Ok(n) = digits.parse::<u32>()
        {
            out.push(n);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Every hand-written source file in the workspace. Not `target`, not the docs, and
/// **not this file** — which has to be able to quote the violations it describes, and
/// whose module note and fixtures are made entirely of them. Excluding it was not a
/// convenience: the first run failed on nine hits and every one was its own prose.
fn sources() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/raw-app -> workspace root")
        .join("crates");
    let mut out = Vec::new();
    walk(&root, &mut out);
    assert!(
        out.len() > 20,
        "found only {} source files under {} — the walk is broken, not the code",
        out.len(),
        root.display()
    );
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs" || e == "wgsl")
            && path
                .file_name()
                .is_some_and(|n| n != "milestone_references.rs")
        {
            out.push(path);
        }
    }
}
