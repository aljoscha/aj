//! Literal term and phrase matching for prose-oriented pick lists.

pub(crate) struct TextQuery {
    terms: Vec<String>,
    phrase: String,
}

impl TextQuery {
    /// Double quotes group a phrase. An unclosed quote extends to the end of
    /// the query so phrases can be searched while they are being typed.
    pub(crate) fn new(query: &str) -> Self {
        let mut terms = Vec::new();
        let mut term = String::new();
        let mut quoted = false;
        for ch in query.chars() {
            if ch == '"' || (ch.is_whitespace() && !quoted) {
                if !term.is_empty() {
                    terms.push(std::mem::take(&mut term).to_lowercase());
                }
                if ch == '"' {
                    quoted = !quoted;
                }
            } else {
                term.push(ch);
            }
        }
        if !term.is_empty() {
            terms.push(term.to_lowercase());
        }
        let phrase = terms.join(" ");
        Self { terms, phrase }
    }

    /// Every term must occur as a case-insensitive substring. A contiguous
    /// occurrence of the terms in query order ranks above scattered terms.
    /// Equal scores leave the caller's source order intact.
    pub(crate) fn score(&self, text: &str) -> Option<u32> {
        if self.terms.is_empty() {
            return Some(0);
        }
        let text = text.to_lowercase();
        self.terms
            .iter()
            .all(|term| text.contains(term))
            .then(|| u32::from(text.contains(&self.phrase)))
    }
}
