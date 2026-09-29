//! What the models know about You: a short "About me" and the project's name and instructions.
//!
//! [`about_you`] builds the text once per meeting (or review); [`render_about`] puts it in a
//! prompt under one heading, so every task reads it the same way and every prompt can refer to it.

/// The heading every prompt uses for the block. Prompts refer to it by name.
pub const ABOUT_HEADING: &str = "## About You and this project";
/// Most characters of the block, so a long note can't crowd out the meeting.
pub const MAX_ABOUT_CHARS: usize = 4_000;

/// The block's text from the user's "About me", the project's name and its instructions. `None`
/// when neither "About me" nor the instructions say anything: a bare project name adds nothing to
/// judge relevance by.
pub fn about_you(about_me: &str, project_name: Option<&str>, instructions: &str) -> Option<String> {
    let about_me = about_me.trim();
    let instructions = instructions.trim();
    if about_me.is_empty() && instructions.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !about_me.is_empty() {
        parts.push(format!("About You: {about_me}"));
    }
    if let Some(name) = project_name.map(str::trim).filter(|n| !n.is_empty()) {
        parts.push(format!("Project: {name}"));
    }
    if !instructions.is_empty() {
        parts.push(format!("Your instructions for this project:\n{instructions}"));
    }
    Some(parts.join("\n").chars().take(MAX_ABOUT_CHARS).collect())
}

/// The block under [`ABOUT_HEADING`], or nothing when there is no text.
pub(crate) fn render_about(text: Option<&str>) -> String {
    match text.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => format!("{ABOUT_HEADING}\n\n{t}\n\n"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_block_names_you_the_project_and_its_instructions() {
        let text = about_you(
            " Solutions engineer at Wisp. ",
            Some("Acme"),
            "I own the migration.\nIgnore billing.",
        )
        .unwrap();
        assert_eq!(
            text,
            "About You: Solutions engineer at Wisp.\nProject: Acme\n\
             Your instructions for this project:\nI own the migration.\nIgnore billing."
        );
        assert_eq!(
            render_about(Some(&text)),
            format!("## About You and this project\n\n{text}\n\n")
        );
    }

    #[test]
    fn nothing_to_say_means_no_block() {
        assert_eq!(about_you("  ", Some("Acme"), ""), None);
        assert_eq!(render_about(None), "");
        assert_eq!(render_about(Some("  ")), "");
        assert_eq!(
            about_you("", None, "Ignore billing.").as_deref(),
            Some("Your instructions for this project:\nIgnore billing.")
        );
    }

    #[test]
    fn a_long_block_is_capped() {
        let long = "x".repeat(MAX_ABOUT_CHARS * 2);
        let text = about_you(&long, None, "").unwrap();
        assert_eq!(text.chars().count(), MAX_ABOUT_CHARS);
    }
}
