//! Deterministic unified diffs for candidate review.
//!
//! The challenger and the repair loop must see the *actual* change, not a
//! path list. This module renders unified hunks (`--- a/path` / `+++ b/path`,
//! `@@ -l,c +l,c @@` headers, 3 lines of context) from recorded file
//! preimages and postimages. Output is bounded: oversized inputs degrade to
//! an explicit `[diff truncated]` marker so evidence never silently omits
//! what the reviewer was shown.
//!
//! The algorithm is a simple O(n·m) LCS on lines — inputs are already
//! scope-bounded by the permit (files/lines caps), so no exotic diff is
//! required. Determinism is the property that matters: identical inputs
//! produce byte-identical output.

/// Context lines rendered around each changed run.
const CONTEXT: usize = 3;
/// Hard cap on total rendered diff size — beyond this we mark truncation.
pub const MAX_DIFF_BYTES: usize = 64 * 1024;
/// Inputs larger than this use a coarse whole-file diff to stay cheap.
const MAX_LCS_CELLS: usize = 2_000_000;

/// A rendered unified diff for one candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffBundle {
    /// The unified diff text (may end with a truncation marker).
    pub text: String,
    /// True when `text` does not cover the complete change.
    pub truncated: bool,
    /// Paths present in the diff, in render order.
    pub files: Vec<String>,
}

/// Render `(path, before, after)` triples as a unified diff.
///
/// `before` is the recorded preimage (empty string for new files); `after`
/// the candidate content. Identical files render headers only.
pub fn render(changes: &[(String, String, String)]) -> DiffBundle {
    let mut out = String::new();
    let mut files = Vec::new();
    let mut truncated = false;
    for (path, before, after) in changes {
        files.push(path.clone());
        let section = diff_file(path, before, after);
        if out.len() + section.len() > MAX_DIFF_BYTES {
            out.push_str(&format!(
                "[diff truncated: {path} and subsequent files exceed {MAX_DIFF_BYTES} bytes]\n"
            ));
            truncated = true;
            break;
        }
        out.push_str(&section);
    }
    DiffBundle {
        text: out,
        truncated,
        files,
    }
}

/// Unified diff for a single file.
fn diff_file(path: &str, before: &str, after: &str) -> String {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    if before == after {
        return out;
    }

    let ops = if old.len() * new.len() > MAX_LCS_CELLS {
        coarse_ops(old.len(), new.len())
    } else {
        lcs_ops(&old, &new)
    };
    for hunk in hunks(&ops) {
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            hunk.old_disp, hunk.old_count, hunk.new_disp, hunk.new_count
        ));
        for op in &ops[hunk.range.clone()] {
            let line = match *op {
                Op::Keep(oi, _) => format!(" {}", old.get(oi).copied().unwrap_or("")),
                Op::Delete(oi) => format!("-{}", old.get(oi).copied().unwrap_or("")),
                Op::Insert(ni) => format!("+{}", new.get(ni).copied().unwrap_or("")),
            };
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// One diff operation over line sequences (indices into old/new).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep(usize, usize),
    Delete(usize),
    Insert(usize),
}

/// Fallback for very large files: replace the entire content in one hunk.
fn coarse_ops(old_len: usize, new_len: usize) -> Vec<Op> {
    let mut ops = Vec::with_capacity(old_len + new_len);
    for i in 0..old_len {
        ops.push(Op::Delete(i));
    }
    for j in 0..new_len {
        ops.push(Op::Insert(j));
    }
    ops
}

/// LCS-based line alignment. Deterministic: ties prefer Delete before
/// Insert (standard unified-diff ordering).
fn lcs_ops(old: &[&str], new: &[&str]) -> Vec<Op> {
    let (n, m) = (old.len(), new.len());
    // table[i][j] = LCS length of old[i..] vs new[j..]
    let mut table = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if old[i] == new[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut ops = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old[i] == new[j] {
            ops.push(Op::Keep(i, j));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            ops.push(Op::Delete(i));
            i += 1;
        } else {
            ops.push(Op::Insert(j));
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Delete(i));
        i += 1;
    }
    while j < m {
        ops.push(Op::Insert(j));
        j += 1;
    }
    ops
}

struct Hunk {
    range: std::ops::Range<usize>,
    old_disp: usize,
    old_count: usize,
    new_disp: usize,
    new_count: usize,
}

/// Group ops into hunks with CONTEXT lines of surrounding context.
///
/// `old_disp`/`new_disp` are 1-based line numbers per unified convention;
/// a zero-count range displays the line *before* the change (e.g. `-0,0`
/// for a new file).
fn hunks(ops: &[Op]) -> Vec<Hunk> {
    let change_idx: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| !matches!(op, Op::Keep(..)))
        .map(|(i, _)| i)
        .collect();
    if change_idx.is_empty() {
        return Vec::new();
    }
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    let mut start = change_idx[0].saturating_sub(CONTEXT);
    let mut end = (change_idx[0] + CONTEXT + 1).min(ops.len());
    for &idx in &change_idx[1..] {
        if idx <= end + CONTEXT {
            end = (idx + CONTEXT + 1).min(ops.len());
        } else {
            ranges.push(start..end);
            start = idx.saturating_sub(CONTEXT);
            end = (idx + CONTEXT + 1).min(ops.len());
        }
    }
    ranges.push(start..end);

    // Cumulative consumed indices before each op position.
    let mut old_before = Vec::with_capacity(ops.len() + 1);
    let mut new_before = Vec::with_capacity(ops.len() + 1);
    let (mut o, mut n) = (0usize, 0usize);
    for op in ops {
        old_before.push(o);
        new_before.push(n);
        match op {
            Op::Keep(..) => {
                o += 1;
                n += 1;
            }
            Op::Delete(_) => o += 1,
            Op::Insert(_) => n += 1,
        }
    }

    ranges
        .into_iter()
        .map(|range| {
            let mut old_count = 0usize;
            let mut new_count = 0usize;
            for op in &ops[range.clone()] {
                match op {
                    Op::Keep(..) => {
                        old_count += 1;
                        new_count += 1;
                    }
                    Op::Delete(_) => old_count += 1,
                    Op::Insert(_) => new_count += 1,
                }
            }
            let old_base = old_before[range.start];
            let new_base = new_before[range.start];
            Hunk {
                range,
                old_disp: if old_count == 0 {
                    old_base
                } else {
                    old_base + 1
                },
                old_count,
                new_disp: if new_count == 0 {
                    new_base
                } else {
                    new_base + 1
                },
                new_count,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str, before: &str, after: &str) -> (String, String, String) {
        (path.into(), before.into(), after.into())
    }

    #[test]
    fn single_line_change_produces_one_hunk() {
        let bundle = render(&[change(
            "src/a.rs",
            "fn a() {}\nlet x = 1;\n",
            "fn a() {}\nlet x = 2;\n",
        )]);
        assert!(!bundle.truncated);
        assert_eq!(bundle.files, vec!["src/a.rs"]);
        assert!(bundle.text.contains("--- a/src/a.rs"));
        assert!(bundle.text.contains("+++ b/src/a.rs"));
        assert!(bundle.text.contains("@@ -1,2 +1,2 @@"));
        assert!(bundle.text.contains(" fn a() {}"));
        assert!(bundle.text.contains("-let x = 1;"));
        assert!(bundle.text.contains("+let x = 2;"));
    }

    #[test]
    fn new_file_renders_all_inserts() {
        let bundle = render(&[change("src/new.rs", "", "fn n() {}\n")]);
        assert!(bundle.text.contains("@@ -0,0 +1,1 @@"));
        assert!(bundle.text.contains("+fn n() {}"));
    }

    #[test]
    fn full_deletion_renders_zero_new_range() {
        let bundle = render(&[change("gone.txt", "a\nb\n", "")]);
        assert!(bundle.text.contains("@@ -1,2 +0,0 @@"));
        assert!(bundle.text.contains("-a"));
    }

    #[test]
    fn distant_changes_split_into_two_hunks() {
        let mut before = String::new();
        for i in 0..20 {
            before.push_str(&format!("line {i}\n"));
        }
        let after = before
            .replacen("line 2", "changed 2", 1)
            .replacen("line 18", "changed 18", 1);
        let bundle = render(&[change("f.txt", &before, &after)]);
        assert_eq!(bundle.text.matches("@@").count() / 2, 2);
        assert!(bundle.text.contains("@@ -1,5 +1,5 @@") || bundle.text.contains("@@ -1,6 +1,6 @@"));
    }

    #[test]
    fn identical_files_render_headers_only() {
        let bundle = render(&[change("f.txt", "same\n", "same\n")]);
        assert!(!bundle.text.contains("@@"));
    }

    #[test]
    fn truncation_is_marked_not_silent() {
        let big = "x".repeat(MAX_DIFF_BYTES);
        let bundle = render(&[change("a.txt", "", &big), change("b.txt", "", "small\n")]);
        assert!(bundle.truncated);
        assert!(bundle.text.contains("[diff truncated"));
    }

    #[test]
    fn insert_at_top_displays_one_based() {
        let bundle = render(&[change("f.txt", "b\nc\n", "a\nb\nc\n")]);
        assert!(bundle.text.contains("@@ -1,2 +1,3 @@"));
        assert!(bundle.text.contains("+a"));
    }
}
