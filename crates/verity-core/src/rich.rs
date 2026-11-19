//! Helpers for Telegram Rich Messages (Bot API 10.1+): 32,768-char limit,
//! GFM-compatible markdown, collapsible thinking blocks, LaTeX.

/// Hard cap for one rich message.
pub const RICH_LIMIT: usize = 32_768;
/// Hard cap for a plain text message.
pub const TEXT_LIMIT: usize = 4_096;

/// Split markdown so each part fits `limit`, breaking on paragraph
/// boundaries when possible and never inside a fenced code block.
pub fn split_markdown(text: &str, limit: usize) -> Vec<String> {
    if text.len() <= limit {
        return vec![text.to_string()];
    }
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_fence = false;
    for para in text.split_inclusive("\n\n") {
        // crude fence tracking: a paragraph containing an odd number of ``` toggles state
        if para.matches("```").count() % 2 == 1 {
            in_fence = !in_fence;
        }
        if cur.len() + para.len() > limit && !cur.is_empty() {
            if in_fence {
                cur.push_str("```\n");
            }
            parts.push(std::mem::take(&mut cur));
            if in_fence {
                cur.push_str("```\n");
            }
        }
        cur.push_str(para);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
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
        let parts = split_markdown(&text, TEXT_LIMIT);
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
}
