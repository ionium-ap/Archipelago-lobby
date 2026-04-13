use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rayon::iter::{ParallelBridge, ParallelIterator};
use similar::{ChangeTag, TextDiff};

use crate::apworld::{FileContent, FileTree};

use super::{
    apply_word_highlighting, fallback_syntax_tokens, find_line_annotations, highlight_hunk_lines,
    Annotations, DiffLine, FileDiff, LineEnding, LineType, TemplateAnnotation,
};

const CONTEXT_COLLAPSE_THRESHOLD: usize = 20;
const CONTEXT_VISIBLE_LINES: usize = 3;

const RENAME_SIMILARITY_THRESHOLD: f32 = 0.5;

pub fn compute_file_tree_diff(
    old_tree: &FileTree,
    new_tree: &FileTree,
    annotations: &BTreeMap<String, Vec<Annotations>>,
) -> Vec<FileDiff> {
    let mut deleted: Vec<&PathBuf> = Vec::new();
    let mut added: Vec<&PathBuf> = Vec::new();
    let mut common: Vec<&PathBuf> = Vec::new();

    for path in old_tree.keys() {
        if new_tree.contains_key(path) {
            common.push(path);
        } else {
            deleted.push(path);
        }
    }
    for path in new_tree.keys() {
        if !old_tree.contains_key(path) {
            added.push(path);
        }
    }

    let renames = detect_renames(old_tree, new_tree, &deleted, &added);
    let renamed_old: BTreeSet<&PathBuf> = renames.iter().map(|(o, _)| *o).collect();
    let renamed_new: BTreeSet<&PathBuf> = renames.iter().map(|(_, n)| *n).collect();

    let all_pairs: Vec<(&PathBuf, &PathBuf)> = common
        .iter()
        .map(|p| (*p, *p))
        .chain(
            deleted
                .iter()
                .filter(|p| !renamed_old.contains(*p))
                .map(|p| (*p, *p)),
        )
        .chain(
            added
                .iter()
                .filter(|p| !renamed_new.contains(*p))
                .map(|p| (*p, *p)),
        )
        .chain(renames.iter().map(|(o, n)| (*o, *n)))
        .collect();

    let mut result: Vec<FileDiff> = all_pairs
        .into_iter()
        .par_bridge()
        .filter_map(|(old_path, new_path)| {
            let old = old_tree.get(old_path);
            let new = new_tree.get(new_path);
            let old_name = old_path.to_string_lossy();
            let new_name = new_path.to_string_lossy();

            let file_annotations: Vec<TemplateAnnotation> = annotations
                .get(new_name.as_ref())
                .map(|anns| {
                    anns.iter()
                        .map(|a| TemplateAnnotation {
                            desc: a.desc.clone(),
                            line: a.line.unwrap_or(0),
                            col_start: a.col_start.unwrap_or(0),
                            col_end: a.col_end.unwrap_or(0),
                        })
                        .collect()
                })
                .unwrap_or_default();

            diff_single_file_renamed(&old_name, &new_name, old, new, &file_annotations)
        })
        .collect();

    result.sort_by(|a, b| a.filename_after.cmp(&b.filename_after));
    result
}

fn detect_renames<'a>(
    old_tree: &FileTree,
    new_tree: &FileTree,
    deleted: &[&'a PathBuf],
    added: &[&'a PathBuf],
) -> Vec<(&'a PathBuf, &'a PathBuf)> {
    if deleted.is_empty() || added.is_empty() {
        return Vec::new();
    }

    let mut renames: Vec<(&'a PathBuf, &'a PathBuf)> = Vec::new();
    let mut matched_old: BTreeSet<&PathBuf> = BTreeSet::new();
    let mut matched_new: BTreeSet<&PathBuf> = BTreeSet::new();

    for &old_path in deleted {
        for &new_path in added {
            if matched_new.contains(new_path) {
                continue;
            }
            if old_tree.get(old_path) == new_tree.get(new_path) {
                renames.push((old_path, new_path));
                matched_old.insert(old_path);
                matched_new.insert(new_path);
                break;
            }
        }
    }

    let remaining_old: Vec<&'a PathBuf> = deleted
        .iter()
        .filter(|p| !matched_old.contains(**p))
        .copied()
        .collect();
    let remaining_new: Vec<&'a PathBuf> = added
        .iter()
        .filter(|p| !matched_new.contains(**p))
        .copied()
        .collect();

    let mut similarity_matches: Vec<(f32, &'a PathBuf, &'a PathBuf)> = Vec::new();
    for &old_path in &remaining_old {
        let Some(FileContent::Text(old_text)) = old_tree.get(old_path) else {
            continue;
        };
        for &new_path in &remaining_new {
            let Some(FileContent::Text(new_text)) = new_tree.get(new_path) else {
                continue;
            };
            let ratio = TextDiff::from_lines(old_text.as_str(), new_text.as_str()).ratio();
            if ratio >= RENAME_SIMILARITY_THRESHOLD {
                similarity_matches.push((ratio, old_path, new_path));
            }
        }
    }

    similarity_matches.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (_, old_path, new_path) in similarity_matches {
        if matched_old.contains(old_path) || matched_new.contains(new_path) {
            continue;
        }
        renames.push((old_path, new_path));
        matched_old.insert(old_path);
        matched_new.insert(new_path);
    }

    renames
}

fn diff_single_file_renamed(
    old_filename: &str,
    new_filename: &str,
    old: Option<&FileContent>,
    new: Option<&FileContent>,
    annotations: &[TemplateAnnotation],
) -> Option<FileDiff> {
    let (filename_before, filename_after) = match (old, new) {
        (None, None) => return None,
        (None, Some(_)) => ("/dev/null".to_string(), new_filename.to_string()),
        (Some(_), None) => (old_filename.to_string(), "/dev/null".to_string()),
        (Some(_), Some(_)) => (old_filename.to_string(), new_filename.to_string()),
    };

    let is_binary =
        matches!(old, Some(FileContent::Binary(_))) || matches!(new, Some(FileContent::Binary(_)));

    if is_binary {
        if old == new && filename_before == filename_after {
            return None;
        }
        return Some(FileDiff {
            filename_before,
            filename_after,
            is_binary: true,
            lines: Vec::new(),
            line_ending_change: None,
        });
    }

    let old_text = match old {
        Some(FileContent::Text(s)) => s.as_str(),
        _ => "",
    };
    let new_text = match new {
        Some(FileContent::Text(s)) => s.as_str(),
        _ => "",
    };

    let old_ending = LineEnding::detect(old_text);
    let new_ending = LineEnding::detect(new_text);
    let line_ending_change = (old.is_some()
        && new.is_some()
        && old_ending != new_ending
        && old_ending != LineEnding::None
        && new_ending != LineEnding::None)
        .then_some((old_ending, new_ending));

    let old_normalized = normalize_line_endings(old_text);
    let new_normalized = normalize_line_endings(new_text);

    if old_normalized == new_normalized {
        if filename_before == filename_after && line_ending_change.is_none() {
            return None;
        }
        return Some(FileDiff {
            filename_before,
            filename_after,
            is_binary: false,
            lines: Vec::new(),
            line_ending_change,
        });
    }

    let mut lines = build_diff_lines(&old_normalized, &new_normalized, new_filename, annotations);
    apply_word_highlighting(&mut lines);
    collapse_context_regions(&mut lines);

    Some(FileDiff {
        filename_before,
        filename_after,
        is_binary: false,
        lines,
        line_ending_change,
    })
}

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn build_diff_lines(
    old_text: &str,
    new_text: &str,
    filename: &str,
    annotations: &[TemplateAnnotation],
) -> Vec<DiffLine> {
    let text_diff = TextDiff::from_lines(old_text, new_text);

    let mut raw_lines: Vec<(String, LineType, (Option<i32>, Option<i32>))> = Vec::new();
    let mut old_num: i32 = 1;
    let mut new_num: i32 = 1;

    for change in text_diff.iter_all_changes() {
        let content = change.value().trim_end_matches(&['\r', '\n']).to_string();
        match change.tag() {
            ChangeTag::Equal => {
                raw_lines.push((content, LineType::Context, (Some(old_num), Some(new_num))));
                old_num += 1;
                new_num += 1;
            }
            ChangeTag::Delete => {
                raw_lines.push((content, LineType::Delete, (Some(old_num), None)));
                old_num += 1;
            }
            ChangeTag::Insert => {
                raw_lines.push((content, LineType::Add, (None, Some(new_num))));
                new_num += 1;
            }
        }
    }

    let syntax_tokens = highlight_hunk_lines(
        &raw_lines
            .iter()
            .map(|(c, lt, _)| (c.clone(), *lt, (0, 0)))
            .collect::<Vec<_>>(),
        filename,
    );

    raw_lines
        .iter()
        .enumerate()
        .map(|(i, (content, line_type, (old_ln, new_ln)))| {
            let tokens = syntax_tokens
                .get(i)
                .cloned()
                .unwrap_or_else(|| fallback_syntax_tokens(content));

            let line_annotations = match (line_type, new_ln) {
                (LineType::Add | LineType::Context, Some(n)) if *n > 0 => {
                    find_line_annotations(*n, annotations)
                }
                _ => Vec::new(),
            };

            DiffLine {
                line_type: *line_type,
                old_line_number: *old_ln,
                new_line_number: *new_ln,
                annotations: line_annotations,
                raw_content: content.clone(),
                syntax_tokens: tokens,
                word_changes: None,
                collapsed: false,
                collapse_count: None,
            }
        })
        .collect()
}

fn collapse_context_regions(lines: &mut [DiffLine]) {
    let mut i = 0;
    while i < lines.len() {
        if lines[i].line_type != LineType::Context {
            i += 1;
            continue;
        }

        let run_start = i;
        while i < lines.len() && lines[i].line_type == LineType::Context {
            i += 1;
        }
        let run_len = i - run_start;

        if run_len <= CONTEXT_COLLAPSE_THRESHOLD {
            continue;
        }

        let collapse_start = run_start + CONTEXT_VISIBLE_LINES;
        let collapse_end = i - CONTEXT_VISIBLE_LINES;

        if collapse_start >= collapse_end {
            continue;
        }

        let hidden_count = collapse_end - collapse_start;
        for line in &mut lines[collapse_start..collapse_end] {
            line.collapsed = true;
        }

        lines[collapse_start].collapse_count = Some(hidden_count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> FileContent {
        FileContent::Text(s.to_string())
    }

    fn make_trees(old: &[(&str, &str)], new: &[(&str, &str)]) -> (FileTree, FileTree) {
        let old_tree: FileTree = old
            .iter()
            .map(|(k, v)| (PathBuf::from(k), text(v)))
            .collect();
        let new_tree: FileTree = new
            .iter()
            .map(|(k, v)| (PathBuf::from(k), text(v)))
            .collect();
        (old_tree, new_tree)
    }

    #[test]
    fn test_new_file() {
        let (old, new) = make_trees(&[], &[("new.py", "print('hello')\n")]);
        let diffs = compute_file_tree_diff(&old, &new, &BTreeMap::new());
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].filename_before, "/dev/null");
        assert_eq!(diffs[0].filename_after, "new.py");
        assert!(diffs[0].lines.iter().all(|l| l.line_type == LineType::Add));
    }

    #[test]
    fn test_removed_file() {
        let (old, new) = make_trees(&[("old.py", "print('bye')\n")], &[]);
        let diffs = compute_file_tree_diff(&old, &new, &BTreeMap::new());
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].filename_before, "old.py");
        assert_eq!(diffs[0].filename_after, "/dev/null");
        assert!(diffs[0]
            .lines
            .iter()
            .all(|l| l.line_type == LineType::Delete));
    }

    #[test]
    fn test_modified_file() {
        let (old, new) = make_trees(
            &[("file.py", "line1\nline2\nline3\n")],
            &[("file.py", "line1\nmodified\nline3\n")],
        );
        let diffs = compute_file_tree_diff(&old, &new, &BTreeMap::new());
        assert_eq!(diffs.len(), 1);

        let lines = &diffs[0].lines;
        assert!(lines.iter().any(|l| l.line_type == LineType::Context));
        assert!(lines.iter().any(|l| l.line_type == LineType::Delete));
        assert!(lines.iter().any(|l| l.line_type == LineType::Add));
    }

    #[test]
    fn test_identical_files_not_shown() {
        let (old, new) = make_trees(&[("same.py", "unchanged\n")], &[("same.py", "unchanged\n")]);
        let diffs = compute_file_tree_diff(&old, &new, &BTreeMap::new());
        assert!(diffs.is_empty());
    }

    #[test]
    fn test_binary_file_unchanged() {
        let hash = [0u8; 32];
        let mut old_tree = FileTree::new();
        let mut new_tree = FileTree::new();
        old_tree.insert("img.png".into(), FileContent::Binary(hash));
        new_tree.insert("img.png".into(), FileContent::Binary(hash));

        let diffs = compute_file_tree_diff(&old_tree, &new_tree, &BTreeMap::new());
        assert!(diffs.is_empty());
    }

    #[test]
    fn test_binary_file_changed() {
        let mut old_tree = FileTree::new();
        let mut new_tree = FileTree::new();
        old_tree.insert("img.png".into(), FileContent::Binary([0u8; 32]));
        new_tree.insert("img.png".into(), FileContent::Binary([1u8; 32]));

        let diffs = compute_file_tree_diff(&old_tree, &new_tree, &BTreeMap::new());
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].is_binary);
    }

    #[test]
    fn test_context_collapsing() {
        let mut old_lines = String::new();
        let mut new_lines = String::new();
        for i in 0..50 {
            old_lines.push_str(&format!("line {i}\n"));
            new_lines.push_str(&format!("line {i}\n"));
        }
        old_lines.push_str("old change\n");
        new_lines.push_str("new change\n");

        let (old, new) = make_trees(&[("big.py", &old_lines)], &[("big.py", &new_lines)]);
        let diffs = compute_file_tree_diff(&old, &new, &BTreeMap::new());
        assert_eq!(diffs.len(), 1);

        let collapsed_count = diffs[0].lines.iter().filter(|l| l.collapsed).count();
        assert!(collapsed_count > 0);

        let visible_context_before_change: Vec<_> = diffs[0]
            .lines
            .iter()
            .filter(|l| l.line_type == LineType::Context && !l.collapsed)
            .collect();
        assert!(visible_context_before_change.len() <= CONTEXT_VISIBLE_LINES * 2 + 2);
    }

    #[test]
    fn test_line_numbers() {
        let (old, new) = make_trees(&[("f.py", "a\nb\nc\n")], &[("f.py", "a\nx\nc\n")]);
        let diffs = compute_file_tree_diff(&old, &new, &BTreeMap::new());
        let lines = &diffs[0].lines;

        // First line: context "a" -> old=1, new=1
        assert_eq!(lines[0].old_line_number, Some(1));
        assert_eq!(lines[0].new_line_number, Some(1));
        assert_eq!(lines[0].line_type, LineType::Context);

        // Delete "b" -> old=2
        assert_eq!(lines[1].old_line_number, Some(2));
        assert_eq!(lines[1].line_type, LineType::Delete);

        // Add "x" -> new=2
        assert_eq!(lines[2].new_line_number, Some(2));
        assert_eq!(lines[2].line_type, LineType::Add);

        // Context "c" -> old=3, new=3
        assert_eq!(lines[3].old_line_number, Some(3));
        assert_eq!(lines[3].new_line_number, Some(3));
        assert_eq!(lines[3].line_type, LineType::Context);
    }

    #[test]
    fn test_rename_exact_match() {
        let (old, new) = make_trees(
            &[("old_name.py", "content\n")],
            &[("new_name.py", "content\n")],
        );
        let diffs = compute_file_tree_diff(&old, &new, &BTreeMap::new());
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].filename_before, "old_name.py");
        assert_eq!(diffs[0].filename_after, "new_name.py");
        assert!(
            diffs[0].lines.is_empty()
                || diffs[0]
                    .lines
                    .iter()
                    .all(|l| l.line_type == LineType::Context)
        );
    }

    #[test]
    fn test_rename_with_modifications() {
        let (old, new) = make_trees(
            &[("old.py", "line1\nline2\nline3\nline4\nline5\n")],
            &[("new.py", "line1\nchanged\nline3\nline4\nline5\n")],
        );
        let diffs = compute_file_tree_diff(&old, &new, &BTreeMap::new());
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].filename_before, "old.py");
        assert_eq!(diffs[0].filename_after, "new.py");
        assert!(diffs[0]
            .lines
            .iter()
            .any(|l| l.line_type == LineType::Delete));
        assert!(diffs[0].lines.iter().any(|l| l.line_type == LineType::Add));
    }

    #[test]
    fn test_no_rename_below_threshold() {
        let (old, new) = make_trees(
            &[("old.py", "completely\ndifferent\ncontent\n")],
            &[("new.py", "nothing\nin\ncommon\nhere\nat\nall\nwhatsoever\n")],
        );
        let diffs = compute_file_tree_diff(&old, &new, &BTreeMap::new());
        assert_eq!(diffs.len(), 2);
    }

    #[test]
    fn test_line_ending_only_change_no_content_diff() {
        let (old, new) = make_trees(&[("f.py", "a\nb\nc\n")], &[("f.py", "a\r\nb\r\nc\r\n")]);
        let diffs = compute_file_tree_diff(&old, &new, &BTreeMap::new());
        assert_eq!(diffs.len(), 1);
        assert_eq!(
            diffs[0].line_ending_change,
            Some((LineEnding::Lf, LineEnding::Crlf))
        );
        assert!(diffs[0].lines.is_empty());
    }

    #[test]
    fn test_line_ending_change_with_content_diff() {
        let (old, new) = make_trees(
            &[("f.py", "a\nb\nc\n")],
            &[("f.py", "a\r\nmodified\r\nc\r\n")],
        );
        let diffs = compute_file_tree_diff(&old, &new, &BTreeMap::new());
        assert_eq!(diffs.len(), 1);
        assert_eq!(
            diffs[0].line_ending_change,
            Some((LineEnding::Lf, LineEnding::Crlf))
        );

        let changed_lines: Vec<_> = diffs[0]
            .lines
            .iter()
            .filter(|l| l.line_type != LineType::Context)
            .collect();
        assert_eq!(
            changed_lines.len(),
            2,
            "only the 'b' -> 'modified' change should appear"
        );
        assert!(changed_lines
            .iter()
            .any(|l| l.line_type == LineType::Delete && l.raw_content == "b"));
        assert!(changed_lines
            .iter()
            .any(|l| l.line_type == LineType::Add && l.raw_content == "modified"));
    }

    #[test]
    fn test_same_line_endings_no_notice() {
        let (old, new) = make_trees(&[("f.py", "a\r\nb\r\n")], &[("f.py", "a\r\nx\r\n")]);
        let diffs = compute_file_tree_diff(&old, &new, &BTreeMap::new());
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].line_ending_change, None);
    }

    #[test]
    fn test_annotations_on_new_lines() {
        let (old, new) = make_trees(&[], &[("f.py", "line1\nline2\nline3\n")]);

        let annotations = BTreeMap::from([(
            "f.py".to_string(),
            vec![Annotations {
                ty: 0,
                desc: "test annotation".into(),
                severity: 1,
                line: Some(2),
                col_start: Some(0),
                col_end: Some(5),
                extra: None,
            }],
        )]);

        let diffs = compute_file_tree_diff(&old, &new, &annotations);
        let annotated: Vec<_> = diffs[0]
            .lines
            .iter()
            .filter(|l| !l.annotations.is_empty())
            .collect();
        assert_eq!(annotated.len(), 1);
        assert_eq!(annotated[0].annotations[0].desc, "test annotation");
    }
}
