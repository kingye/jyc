//! Recovery for `ask_user` calls that a model wrote as XML in its reply
//! text instead of emitting a native tool call.
//!
//! Models with weak function-calling (see the provider adapter notes)
//! sometimes end their reply with a literal `<ask_user question="..." ...>`
//! tag. Left in place, the auto-delivery fallback ships the raw syntax to
//! the user and the question never executes. This module finds such a tag,
//! classifies it as well-formed or malformed, and computes the byte span
//! to remove from the delivered text.

/// An `ask_user` tag embedded in assistant reply text.
pub(crate) enum EmbeddedAsk {
    /// Parseable invocation — safe to execute as the real tool.
    WellFormed {
        /// Byte span of the whole `<ask_user ...>` tag in the original text.
        span: std::ops::Range<usize>,
        question: String,
        options: Vec<String>,
        timeout_secs: Option<u64>,
    },
    /// Tool-looking tag that could not be parsed (bad quoting, missing
    /// attributes, unterminated...). Only the span is trustworthy: strip
    /// it so raw syntax never reaches the user.
    Malformed { span: std::ops::Range<usize> },
}

const TAG: &str = "<ask_user";

/// Locate an embedded `<ask_user ...>` tag in `text`.
///
/// Returns `None` when no tag is present. A tag is recognized by its
/// literal `<ask_user` opener followed by whitespace, `>`, or end of
/// input — closing tags (`</ask_user>`) and longer tool names do not
/// match.
pub(crate) fn find_embedded_ask(text: &str) -> Option<EmbeddedAsk> {
    let start = find_tag_start(text)?;
    // The tag ends at the first `>` outside double quotes; a quote left
    // open swallows the rest of the text, which is then treated as one
    // malformed tag.
    let (inner_end, terminated) = match find_tag_end(text, start + TAG.len()) {
        Some(end) => (end, true),
        None => (text.len(), false),
    };
    let span = start..if terminated {
        inner_end + 1
    } else {
        text.len()
    };
    let inner = &text[start + 1..inner_end];
    if !terminated {
        return Some(EmbeddedAsk::Malformed { span });
    }
    match parse_inner(inner) {
        Some((question, options, timeout_secs)) => Some(EmbeddedAsk::WellFormed {
            span,
            question,
            options,
            timeout_secs,
        }),
        None => Some(EmbeddedAsk::Malformed { span }),
    }
}

fn find_tag_start(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(pos) = text[from..].find(TAG) {
        let start = from + pos;
        let after = start + TAG.len();
        let boundary = text[after..]
            .chars()
            .next()
            .map_or(true, |c| c.is_whitespace() || c == '>' || c == '/');
        if boundary {
            return Some(start);
        }
        from = after;
    }
    None
}

/// First `>` at quote-depth zero scanning from `from`; `None` when a
/// quote stays open (unterminated tag).
fn find_tag_end(text: &str, from: usize) -> Option<usize> {
    let mut in_quote = false;
    for (i, c) in text.char_indices().skip_while(|(i, _)| *i < from) {
        match c {
            '"' => in_quote = !in_quote,
            '>' if !in_quote => return Some(i),
            _ => {}
        }
    }
    None
}

/// Parse the tag interior (`ask_user question="..." options="..."`).
/// Returns `None` when required attributes are missing or empty.
fn parse_inner(inner: &str) -> Option<(String, Vec<String>, Option<u64>)> {
    let question = extract_attr(inner, "question")?.trim().to_string();
    if question.is_empty() {
        return None;
    }
    let options = extract_options(inner)?;
    let timeout_secs = extract_attr(inner, "timeout_seconds")
        .or_else(|| extract_attr(inner, "timeout"))
        .and_then(|v| v.trim().parse::<u64>().ok());
    Some((question, options, timeout_secs))
}

/// Value of a whitespace-delimited `key="value"` attribute. The leading
/// space in the needle keeps the search off values that merely contain the
/// key as a substring — e.g. a question asking which "options" to pick
/// must not be read as the options attribute.
fn extract_attr<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=\"");
    let pos = text.find(&needle)?;
    let after = &text[pos + needle.len()..];
    let end = after.find('"')?;
    Some(&after[..end])
}

/// Options list, tolerant of the two styles models actually write:
/// comma-separated inside one pair of quotes (`options="a, b"`) and the
/// misquoted per-item style (`options="a", "b"`).
fn extract_options(inner: &str) -> Option<Vec<String>> {
    // Whitespace-delimited needle — see `extract_attr` for why.
    let needle = " options=\"";
    let pos = inner.find(needle)?;
    // Everything between the opening quote and the end of the tag.
    let region = &inner[pos + needle.len()..];
    let items: Vec<String> = if region.contains('"') {
        // Misquoted per-item style: `a", "b", "c` → quoted segments.
        let mut items = Vec::new();
        let mut rest = region;
        while let Some(open) = rest.find('"') {
            let tail = &rest[open + 1..];
            let Some(close) = tail.find('"') else { break };
            let item = tail[..close].trim();
            if !item.is_empty() {
                items.push(item.to_string());
            }
            rest = &tail[close + 1..];
        }
        items
    } else {
        region
            .split(',')
            .map(|s| s.trim().trim_matches('"').trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    if items.is_empty() { None } else { Some(items) }
}

/// Remove `span` from `text`, cleaning up the seam so no dangling blank
/// lines or stray whitespace remain.
pub(crate) fn remove_span(text: &str, span: std::ops::Range<usize>) -> String {
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..span.start]);
    out.push_str(&text[span.end..]);
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn well_formed(text: &str) -> Option<(String, Vec<String>, Option<u64>)> {
        match find_embedded_ask(text)? {
            EmbeddedAsk::WellFormed {
                question,
                options,
                timeout_secs,
                ..
            } => Some((question, options, timeout_secs)),
            EmbeddedAsk::Malformed { .. } => None,
        }
    }

    #[test]
    fn parses_documented_form() {
        let ask = well_formed(
            "方案如上。\n<ask_user question=\"开工吗？\" options=\"按方案, 再想想\" timeout_seconds=\"60\">",
        )
        .expect("documented comma-separated form must parse");
        assert_eq!(ask.0, "开工吗？");
        assert_eq!(ask.1, vec!["按方案".to_string(), "再想想".to_string()]);
        assert_eq!(ask.2, Some(60));
    }

    #[test]
    fn parses_misquoted_per_item_form() {
        // The exact failure from the field: per-item quotes outside the
        // attribute. Tolerated by sifting quoted segments.
        let ask = well_formed(
            "<ask_user question=\"修复方案如上，是否开工？\" options=\"按方案 1+2+3 全部开工\", \"只修根因 1，2/3 以后再说\", \"方案要调整（回复说明）\">",
        )
        .expect("misquoted per-item form must still parse");
        assert_eq!(ask.0, "修复方案如上，是否开工？");
        assert_eq!(
            ask.1,
            vec![
                "按方案 1+2+3 全部开工".to_string(),
                "只修根因 1，2/3 以后再说".to_string(),
                "方案要调整（回复说明）".to_string(),
            ]
        );
        assert_eq!(ask.2, None);
    }

    #[test]
    fn attribute_values_containing_key_words_do_not_confuse_the_parser() {
        // Regression: a question asking about "options" must not be read
        // as the options attribute (substring match inside the value).
        let ask = well_formed(
            "<ask_user question=\"这两个 options 选哪个？\" options=\"a, b\" timeout_seconds=\"30\">",
        )
        .expect("question text mentioning options must still parse");
        assert_eq!(ask.0, "这两个 options 选哪个？");
        assert_eq!(ask.1, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(ask.2, Some(30));

        let ask = well_formed("<ask_user options=\"question, 其他\" question=\"q\">")
            .expect("options text mentioning question must still parse");
        assert_eq!(ask.0, "q");
        assert_eq!(ask.1, vec!["question".to_string(), "其他".to_string()]);
    }

    #[test]
    fn missing_question_is_malformed() {
        match find_embedded_ask("<ask_user options=\"a, b\">").expect("tag must be found") {
            EmbeddedAsk::Malformed { span } => {
                assert_eq!(span, 0.."<ask_user options=\"a, b\">".len());
            }
            _ => panic!("missing question must be malformed"),
        }
    }

    #[test]
    fn unterminated_tag_is_malformed_to_end_of_text() {
        let text = "prose\n<ask_user question=\"q\" options=\"a";
        match find_embedded_ask(text).expect("tag must be found") {
            EmbeddedAsk::Malformed { span } => assert_eq!(span, 6..text.len()),
            _ => panic!("unterminated tag must be malformed"),
        }
    }

    #[test]
    fn no_tag_returns_none() {
        assert!(find_embedded_ask("普通回复，没有工具调用。").is_none());
        assert!(find_embedded_ask("看看 </ask_user> 这个闭合影不影响").is_none());
        assert!(find_embedded_ask("提一嘴 <ask_user_profile> 别的工具").is_none());
    }

    #[test]
    fn remove_span_cleans_the_seam() {
        let text = "第一行方案。\n\n<ask_user question=\"q\" options=\"a, b\">\n";
        let ask = find_embedded_ask(text).expect("tag must be found");
        let EmbeddedAsk::WellFormed { span, .. } = ask else {
            panic!("must be well-formed");
        };
        assert_eq!(remove_span(text, span), "第一行方案。");
    }
}
