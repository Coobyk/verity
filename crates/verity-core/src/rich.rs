//! Helpers for Telegram Rich Messages (Bot API 10.1+): 32,768-char limit,
//! GFM-compatible markdown, collapsible thinking blocks, LaTeX.

/// Hard cap for one rich message, in Unicode characters.
pub const RICH_LIMIT: usize = 32_768;
/// Hard cap for a plain text message, in UTF-16 code units.
pub const TEXT_LIMIT: usize = 4_096;

/// Split rich markdown at paragraph, line, or word boundaries where possible.
/// Fenced blocks are kept together or closed and reopened with their info string.
pub fn split_markdown(text: &str, limit: usize) -> Vec<String> {
    split_with_units(text, limit, |_| 1)
}

/// Split legacy markdown within Telegram's UTF-16 plain-message limit.
pub fn split_markdown_utf16(text: &str, limit: usize) -> Vec<String> {
    split_with_units(text, limit, char::len_utf16)
}

fn split_with_units(text: &str, limit: usize, units: fn(char) -> usize) -> Vec<String> {
    debug_assert!(limit >= 2);
    let len = |s: &str| s.chars().map(units).sum::<usize>();
    if len(text) <= limit {
        return vec![text.to_string()];
    }
    let mut parts = Vec::new();
    let mut fence: Option<(usize, usize, &str)> = None;
    let mut plain_start = 0;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let end = offset + line.len();
        if let Some((marker, info)) = fence_marker(line) {
            if let Some((start, body_start, opening_marker)) = fence {
                if marker.starts_with(opening_marker) && info.trim().is_empty() {
                    if plain_start < start {
                        append_plain(&mut parts, &text[plain_start..start], limit, units);
                    }
                    let block = &text[start..end];
                    if len(block) <= limit {
                        append_piece(&mut parts, block.to_string(), limit, units);
                    } else {
                        for chunk in split_fence_block(
                            &text[start..body_start],
                            &text[body_start..offset],
                            line,
                            limit,
                            units,
                        ) {
                            append_piece(&mut parts, chunk, limit, units);
                        }
                    }
                    plain_start = end;
                    fence = None;
                }
            } else if !marker.starts_with('`') || !info.contains('`') {
                fence = Some((offset, end, marker));
            }
        }
        offset = end;
    }
    if plain_start < text.len() {
        append_plain(&mut parts, &text[plain_start..], limit, units);
    }
    parts
}

fn split_fence_block(
    opening: &str,
    body: &str,
    closing: &str,
    limit: usize,
    units: fn(char) -> usize,
) -> Vec<String> {
    let opening_marker = fence_marker(opening).unwrap().0;
    let closing_marker = fence_marker(closing).unwrap().0;
    let fence_char = opening_marker.chars().next().unwrap();
    let longest_run = body
        .split(|c| c != fence_char)
        .map(str::len)
        .max()
        .unwrap_or(0);
    let delimiter = fence_char
        .to_string()
        .repeat(opening_marker.len().max(longest_run + 1));
    let opening = opening.replacen(opening_marker, &delimiter, 1);
    let closing = closing.replacen(closing_marker, &delimiter, 1);
    let overhead = opening
        .chars()
        .chain(closing.chars())
        .map(units)
        .sum::<usize>()
        + 1;
    if overhead + 2 > limit {
        return split_plain(body, limit, units);
    }
    split_plain(body, limit - overhead, units)
        .into_iter()
        .map(|chunk| {
            let newline = if chunk.ends_with('\n') { "" } else { "\n" };
            format!("{opening}{chunk}{newline}{closing}")
        })
        .collect()
}

fn append_piece(parts: &mut Vec<String>, piece: String, limit: usize, units: fn(char) -> usize) {
    if let Some(last) = parts.last_mut() {
        let newline = !last.ends_with('\n')
            && fence_marker(last.lines().last().unwrap_or("")).is_some()
            && fence_marker(piece.lines().next().unwrap_or("")).is_some();
        if last.chars().chain(piece.chars()).map(units).sum::<usize>() + usize::from(newline)
            <= limit
        {
            if newline {
                last.push('\n');
            }
            last.push_str(&piece);
            return;
        }
    }
    parts.push(piece);
}

fn append_plain(parts: &mut Vec<String>, mut text: &str, limit: usize, units: fn(char) -> usize) {
    if text.is_empty() {
        return;
    }
    let mut remaining = text.chars().map(units).sum::<usize>();
    if let Some(last) = parts.last_mut() {
        let mut available = limit - last.chars().map(units).sum::<usize>();
        while remaining > limit && available >= 2 {
            let end = split_point(text, available, units);
            let used = text[..end].chars().map(units).sum::<usize>();
            last.push_str(&text[..end]);
            text = &text[end..];
            available -= used;
            remaining -= used;
        }
    }
    if remaining <= limit {
        append_piece(parts, text.to_string(), limit, units);
    } else {
        parts.extend(split_plain(text, limit, units));
    }
}

fn fence_marker(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let marker = trimmed.chars().next()?;
    if !matches!(marker, '`' | '~') {
        return None;
    }
    let count = trimmed.chars().take_while(|&c| c == marker).count();
    (count >= 3).then(|| trimmed.split_at(count))
}

fn split_plain(mut text: &str, limit: usize, units: fn(char) -> usize) -> Vec<String> {
    let mut parts = Vec::new();
    while !text.is_empty() {
        let end = split_point(text, limit, units);
        parts.push(text[..end].to_string());
        text = &text[end..];
    }
    parts
}

fn split_point(text: &str, limit: usize, units: fn(char) -> usize) -> usize {
    let mut used = 0;
    let mut end = 0;
    let (mut paragraph, mut line, mut whitespace) = (None, None, None);
    for (i, ch) in text.char_indices() {
        if used + units(ch) > limit {
            break;
        }
        used += units(ch);
        end = i + ch.len_utf8();
        if ch.is_whitespace() {
            whitespace = Some(end);
        }
        if ch == '\n' {
            line = Some(end);
            if text[..end].ends_with("\n\n") {
                paragraph = Some(end);
            }
        }
    }
    if end < text.len() {
        end = paragraph.or(line).or(whitespace).unwrap_or(end);
    }
    // The bridge's limits always fit a complete Unicode character.
    if end == 0 {
        end = text.chars().next().unwrap().len_utf8();
    }
    end
}

/// Wrap agent reasoning in a collapsible thinking block (Rich HTML).
pub fn thinking_block(markdown: &str) -> String {
    format!("<details><summary>Thinking</summary>\n\n{markdown}\n\n</details>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_unsplit() {
        assert_eq!(split_markdown("hello", 100), vec!["hello"]);
    }

    #[test]
    fn splits_on_paragraphs() {
        let text = "a".repeat(60) + "\n\n" + &"b".repeat(60) + "\n\n" + &"c".repeat(60);
        let parts = split_markdown(&text, 100);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.len() <= 100));
        assert_eq!(parts.concat().replace("\n", ""), text.replace("\n", ""));
    }

    #[test]
    fn packs_plain_text_and_whole_fences_greedily() {
        let text = "intro\n```rs\nx\n```\noutro";
        assert_eq!(
            split_markdown(text, 20),
            vec!["intro\n```rs\nx\n```\n", "outro"]
        );
    }

    #[test]
    fn near_limit_reply_with_short_fences_needs_only_two_parts() {
        let text = format!(
            "intro\n{}{}",
            "```rs\nx\n```\nparagraph\n".repeat(5),
            "word ".repeat(6_600)
        );
        let parts = split_markdown(&text, RICH_LIMIT);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts.concat(), text);
        assert!(parts.iter().all(|p| p.chars().count() <= RICH_LIMIT));
    }

    #[test]
    fn splits_long_unbroken_paragraph() {
        let text = "x".repeat(RICH_LIMIT + 1);
        let parts = split_markdown(&text, RICH_LIMIT);
        assert_eq!(parts.concat(), text);
        assert!(parts.iter().all(|p| p.chars().count() <= RICH_LIMIT));
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn splits_long_paragraph_at_whitespace() {
        let text = "hello world again";
        assert_eq!(split_markdown(text, 12), vec!["hello world ", "again"]);
        assert_eq!(
            split_markdown("hello\nworld again", 12),
            vec!["hello\n", "world again"]
        );
    }

    #[test]
    fn rich_limit_counts_characters_not_bytes() {
        let text = "é😀".repeat(RICH_LIMIT / 2);
        assert_eq!(split_markdown(&text, RICH_LIMIT), vec![text]);
        let text = "é".repeat(RICH_LIMIT - 1) + "😀z";
        let parts = split_markdown(&text, RICH_LIMIT);
        assert_eq!(parts.concat(), text);
        assert_eq!(parts[0].chars().count(), RICH_LIMIT);
        assert!(parts[0].ends_with('😀'));
    }

    #[test]
    fn plain_limit_counts_utf16_without_splitting_emoji() {
        let text = "x".repeat(TEXT_LIMIT - 1) + "😀z";
        let parts = split_markdown_utf16(&text, TEXT_LIMIT);
        assert_eq!(parts.concat(), text);
        assert!(parts.iter().all(|p| p.encode_utf16().count() <= TEXT_LIMIT));
        assert!(parts[1].starts_with('😀'));
    }

    #[test]
    fn split_fence_preserves_info_and_balances_parts() {
        let text = format!("```rust linenos\n{}\n```", "x".repeat(80));
        let parts = split_markdown(&text, 40);
        assert!(parts.len() > 1);
        for part in &parts {
            assert!(part.chars().count() <= 40, "{part:?}");
            assert!(part.starts_with("```rust linenos\n"), "{part:?}");
            assert!(part.ends_with("\n```"), "{part:?}");
            assert_eq!(part.matches("```").count(), 2, "{part:?}");
        }
        let body: String = parts
            .iter()
            .map(|p| {
                p.strip_prefix("```rust linenos\n")
                    .unwrap()
                    .strip_suffix("\n```")
                    .unwrap()
            })
            .collect();
        assert_eq!(body, "x".repeat(80));
    }

    #[test]
    fn fence_closing_paragraph_is_balanced() {
        let parts = split_markdown("```rust\nx\n\ny\n```", 14);
        assert!(parts.len() > 1);
        assert!(parts
            .iter()
            .all(|p| p.starts_with("```rust\n") && p.ends_with("\n```")));
        assert!(parts.iter().all(|p| p.chars().count() <= 14));
    }

    #[test]
    fn empty_and_short_unclosed_fence_are_unchanged() {
        assert_eq!(split_markdown("", RICH_LIMIT), vec![""]);
        assert_eq!(
            split_markdown("```rust\nhello", RICH_LIMIT),
            vec!["```rust\nhello"]
        );
    }

    #[test]
    fn recognizes_fence_lines_not_inline_backticks() {
        let text = "inline ``` text\nnext line here";
        assert_eq!(split_markdown(text, 20).concat(), text);
        for marker in ["~~~~", "````"] {
            let text = format!("before\n{marker}rust\n{}\n{marker}\nafter", "😀".repeat(40));
            let parts = split_markdown_utf16(&text, 40);
            assert!(parts.iter().all(|p| p.encode_utf16().count() <= 40));
            let code: Vec<_> = parts
                .iter()
                .filter(|p| p.lines().any(|line| line.starts_with(marker)))
                .collect();
            assert!(code.len() > 1);
            for part in code {
                assert!(part.lines().any(|line| line == format!("{marker}rust")));
                assert_eq!(
                    part.lines().filter(|line| line.starts_with(marker)).count() % 2,
                    0
                );
            }
        }
    }

    #[test]
    fn oversized_fence_wrappers_are_dropped() {
        let text = format!("```{}\nhello world\n```", "r".repeat(40));
        let parts = split_markdown(&text, 10);
        assert_eq!(parts.concat(), "hello world\n");
        assert!(parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().count() <= 10));
    }

    #[test]
    fn cuts_cannot_turn_code_into_a_closing_fence() {
        let text = format!("```md\n{}\n```", "```not_a_close\n".repeat(10));
        let parts = split_markdown(&text, 30);
        for part in parts {
            assert!(part.chars().count() <= 30);
            assert!(part.starts_with("````md\n"), "{part:?}");
            assert!(part.ends_with("\n````"), "{part:?}");
        }
    }
}
