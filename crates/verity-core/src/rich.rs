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
}
