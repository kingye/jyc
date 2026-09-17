//! Feishu card for pending `ask_user` questions.
//!
//! Renders a question with numbered options as a card JSON 2.0 interactive
//! message. Button callbacks (`card.action.trigger`) cannot be received
//! through openlark-client's WebSocket layer (card frames are dropped by its
//! frame handler), so the card presents numbered options and the user
//! answers by replying with the number or the option text; the feishu pipe
//! routes that reply to the shared question hub instead of the topic.

/// Build the card JSON 2.0 envelope for a pending question.
///
/// `question` is the agent's question text; `options` are the selectable
/// answers rendered as a numbered list. The hint footer tells the user to
/// reply with the number or option text.
pub fn build_question_card(question: &str, options: &[String]) -> serde_json::Value {
    let mut elements = vec![serde_json::json!({
        "tag": "markdown",
        "content": question
    })];

    if !options.is_empty() {
        let numbered: Vec<String> = options
            .iter()
            .enumerate()
            .map(|(i, opt)| format!("{}. {}", i + 1, opt))
            .collect();
        elements.push(serde_json::json!({
            "tag": "markdown",
            "content": numbered.join("\n")
        }));
    }

    elements.push(serde_json::json!({
        "tag": "markdown",
        "content": "👉 回复序号或选项内容作答"
    }));

    serde_json::json!({
        "schema": "2.0",
        "config": { "enable_forward": true, "update_multi": true },
        "header": {
            "title": { "tag": "plain_text", "content": "❓ 需要你的选择" },
            "template": "blue"
        },
        "body": { "elements": elements }
    })
}

#[cfg(test)]
mod tests {
    use super::build_question_card;

    fn elements(card: &serde_json::Value) -> &Vec<serde_json::Value> {
        card["body"]["elements"].as_array().unwrap()
    }

    #[test]
    fn card_uses_json_2_0_envelope_with_header() {
        let card = build_question_card("q", &["a".to_string()]);
        assert_eq!(card["schema"], "2.0");
        assert_eq!(card["config"]["enable_forward"], true);
        assert_eq!(card["header"]["title"]["content"], "❓ 需要你的选择");
    }

    #[test]
    fn options_rendered_numbered_after_question() {
        let card = build_question_card("pick one", &["foo".to_string(), "bar".to_string()]);
        let els = elements(&card);
        assert_eq!(els[0]["content"], "pick one");
        assert_eq!(els[1]["content"], "1. foo\n2. bar");
    }

    #[test]
    fn empty_options_skip_numbering_but_keep_hint() {
        let card = build_question_card("free form?", &[]);
        let els = elements(&card);
        assert_eq!(els.len(), 2);
        assert_eq!(els[1]["content"], "👉 回复序号或选项内容作答");
    }

    #[test]
    fn question_text_is_not_renumbered() {
        // A question containing digits must render verbatim in element 0.
        let card = build_question_card("choose 2 things", &["x".to_string()]);
        assert_eq!(elements(&card)[0]["content"], "choose 2 things");
    }
}
