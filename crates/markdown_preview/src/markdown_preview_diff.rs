use std::ops::Range;

/// A git hunk, as byte ranges in the current text and in the committed text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hunk {
    pub new: Range<usize>,
    pub old: Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// Root blocks changed by one or more hunks. Git compares lines while the
/// preview shows blocks, so a change always covers whole blocks on both sides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BlockChange {
    pub kind: ChangeKind,
    /// The changed root blocks of the current text. For a deletion, it is
    /// empty and starts at the block shown after the deleted blocks.
    pub blocks: Range<usize>,
    /// Source range of the changed blocks in the current text.
    pub new_range: Range<usize>,
    /// Source range of the changed blocks in the committed text.
    pub old_range: Range<usize>,
}

/// Groups hunks into changes of whole root blocks. `hunks` must be sorted and
/// must not overlap.
pub(crate) fn block_changes(
    new_blocks: &[Range<usize>],
    old_blocks: &[Range<usize>],
    hunks: &[Hunk],
) -> Vec<BlockChange> {
    let mut groups: Vec<Hunk> = hunks.to_vec();
    loop {
        let mut grown: Vec<Hunk> = Vec::with_capacity(groups.len());
        for group in &groups {
            let new = union(
                expand_to_blocks(&group.new, new_blocks),
                map_range(&expand_to_blocks(&group.old, old_blocks), hunks, Side::Old),
            );
            let old = union(
                expand_to_blocks(&group.old, old_blocks),
                map_range(&expand_to_blocks(&group.new, new_blocks), hunks, Side::New),
            );
            let group = Hunk { new, old };
            match grown.last_mut() {
                Some(last) if touches(&last.new, &group.new) || touches(&last.old, &group.old) => {
                    *last = Hunk {
                        new: union(last.new.clone(), group.new),
                        old: union(last.old.clone(), group.old),
                    };
                }
                _ => grown.push(group),
            }
        }
        if grown == groups {
            break;
        }
        groups = grown;
    }

    groups
        .into_iter()
        .filter_map(|group| {
            let blocks = intersecting_blocks(&group.new, new_blocks);
            let has_old_blocks = !intersecting_blocks(&group.old, old_blocks).is_empty();
            let kind = match (blocks.is_empty(), has_old_blocks) {
                (false, false) => ChangeKind::Added,
                (false, true) => ChangeKind::Modified,
                (true, true) => ChangeKind::Deleted,
                // Only blank lines changed.
                (true, false) => return None,
            };
            let blocks = if blocks.is_empty() {
                let next_block = new_blocks.partition_point(|block| block.start < group.new.start);
                next_block..next_block
            } else {
                blocks
            };
            Some(BlockChange {
                kind,
                new_range: span_of_blocks(&group.new, new_blocks),
                old_range: span_of_blocks(&group.old, old_blocks),
                blocks,
            })
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Side {
    New,
    Old,
}

/// Maps a range of one side to the other side. Offsets outside of hunks keep
/// their distance to the end of the previous hunk, and offsets inside a hunk
/// map to the matching end of the hunk on the other side.
fn map_range(range: &Range<usize>, hunks: &[Hunk], from: Side) -> Range<usize> {
    map_offset(range.start, hunks, from, true)..map_offset(range.end, hunks, from, false)
}

fn map_offset(offset: usize, hunks: &[Hunk], from: Side, is_start: bool) -> usize {
    let mut mapped = offset;
    for hunk in hunks {
        let (source_range, target_range) = match from {
            Side::New => (&hunk.new, &hunk.old),
            Side::Old => (&hunk.old, &hunk.new),
        };
        if offset < source_range.start || (offset == source_range.start && is_start) {
            break;
        }
        if offset < source_range.end || (offset == source_range.end && !is_start) {
            return if is_start {
                target_range.start
            } else {
                target_range.end
            };
        }
        mapped = target_range.end + (offset - source_range.end);
    }
    mapped
}

/// Grows `range` to cover every block it intersects. An empty range only
/// grows when it lies strictly inside a block.
fn expand_to_blocks(range: &Range<usize>, blocks: &[Range<usize>]) -> Range<usize> {
    let intersecting = intersecting_blocks(range, blocks);
    if intersecting.is_empty() {
        return range.clone();
    }
    union(
        range.clone(),
        blocks[intersecting.start].start..blocks[intersecting.end - 1].end,
    )
}

/// Source range from the first to the last block intersecting `range`, or
/// the empty range at its start when there is none.
fn span_of_blocks(range: &Range<usize>, blocks: &[Range<usize>]) -> Range<usize> {
    let intersecting = intersecting_blocks(range, blocks);
    if intersecting.is_empty() {
        return range.start..range.start;
    }
    blocks[intersecting.start].start..blocks[intersecting.end - 1].end
}

fn intersecting_blocks(range: &Range<usize>, blocks: &[Range<usize>]) -> Range<usize> {
    let intersects = |block: &Range<usize>| {
        if range.is_empty() {
            block.start < range.start && range.start < block.end
        } else {
            block.start < range.end && range.start < block.end
        }
    };
    let first = blocks.partition_point(|block| block.end <= range.start);
    let mut end = first;
    while end < blocks.len() && intersects(&blocks[end]) {
        end += 1;
    }
    first..end
}

fn union(a: Range<usize>, b: Range<usize>) -> Range<usize> {
    a.start.min(b.start)..a.end.max(b.end)
}

fn touches(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes(
        old_text: &str,
        new_text: &str,
        hunks: &[Hunk],
    ) -> Vec<(ChangeKind, String, String)> {
        let new_blocks = markdown::root_block_ranges(new_text);
        let old_blocks = markdown::root_block_ranges(old_text);
        block_changes(&new_blocks, &old_blocks, hunks)
            .into_iter()
            .map(|change| {
                (
                    change.kind,
                    old_text[change.old_range].trim_end().to_string(),
                    new_text[change.new_range].trim_end().to_string(),
                )
            })
            .collect()
    }

    /// Line-based hunks between `old_text` and `new_text`, like git computes.
    fn line_hunks(old_text: &str, new_text: &str) -> Vec<Hunk> {
        let old_lines: Vec<&str> = old_text.split_inclusive('\n').collect();
        let new_lines: Vec<&str> = new_text.split_inclusive('\n').collect();
        let prefix = old_lines
            .iter()
            .zip(&new_lines)
            .take_while(|(old, new)| old == new)
            .count();
        let suffix = old_lines[prefix..]
            .iter()
            .rev()
            .zip(new_lines[prefix..].iter().rev())
            .take_while(|(old, new)| old == new)
            .count();
        let offset = |lines: &[&str], line: usize| -> usize {
            lines[..line].iter().map(|line| line.len()).sum()
        };
        if prefix == old_lines.len() && prefix == new_lines.len() {
            return Vec::new();
        }
        vec![Hunk {
            new: offset(&new_lines, prefix)..offset(&new_lines, new_lines.len() - suffix),
            old: offset(&old_lines, prefix)..offset(&old_lines, old_lines.len() - suffix),
        }]
    }

    fn diff(old_text: &str, new_text: &str) -> Vec<(ChangeKind, String, String)> {
        changes(old_text, new_text, &line_hunks(old_text, new_text))
    }

    #[test]
    fn test_added_block() {
        assert_eq!(
            diff("# Title\n\nFirst.\n", "# Title\n\nNew.\n\nFirst.\n"),
            vec![(ChangeKind::Added, String::new(), "New.".to_string())]
        );
    }

    #[test]
    fn test_deleted_block() {
        assert_eq!(
            diff("# Title\n\nGone.\n\nFirst.\n", "# Title\n\nFirst.\n"),
            vec![(ChangeKind::Deleted, "Gone.".to_string(), String::new())]
        );
        let new_text = "# Title\n\nFirst.\n";
        let old_text = "# Title\n\nGone.\n\nFirst.\n";
        let change = &block_changes(
            &markdown::root_block_ranges(new_text),
            &markdown::root_block_ranges(old_text),
            &line_hunks(old_text, new_text),
        )[0];
        assert_eq!(change.blocks, 1..1);
    }

    #[test]
    fn test_modified_line_covers_whole_list() {
        assert_eq!(
            diff(
                "- one\n- two\n- three\n\nAfter.\n",
                "- one\n- TWO\n- three\n\nAfter.\n"
            ),
            vec![(
                ChangeKind::Modified,
                "- one\n- two\n- three".to_string(),
                "- one\n- TWO\n- three".to_string()
            )]
        );
    }

    #[test]
    fn test_blank_lines_only() {
        assert_eq!(diff("First.\n\nSecond.\n", "First.\n\n\nSecond.\n"), vec![]);
    }

    #[test]
    fn test_separate_changes() {
        let old_text = "One.\n\nTwo.\n\nThree.\n";
        let new_text = "One!\n\nTwo.\n\nThree!\n";
        let hunks = vec![
            Hunk {
                new: 0..5,
                old: 0..5,
            },
            Hunk {
                new: 12..19,
                old: 12..19,
            },
        ];
        assert_eq!(
            changes(old_text, new_text, &hunks),
            vec![
                (ChangeKind::Modified, "One.".to_string(), "One!".to_string()),
                (
                    ChangeKind::Modified,
                    "Three.".to_string(),
                    "Three!".to_string()
                ),
            ]
        );
    }

    #[test]
    fn test_paragraph_split_in_two() {
        assert_eq!(
            diff(
                "Line one\nline two\n\nEnd.\n",
                "Line one\n\nline two\n\nEnd.\n"
            ),
            vec![(
                ChangeKind::Modified,
                "Line one\nline two".to_string(),
                "Line one\n\nline two".to_string()
            )]
        );
    }
}
