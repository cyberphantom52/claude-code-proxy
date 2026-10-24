use super::reducer::TextDelta;

// Local projection limits, not upstream Codex capabilities.
pub(super) const MAX_STOP_SEQUENCES: usize = 64;
pub(super) const MAX_STOP_BYTES: usize = 4096;

#[derive(Clone, Default)]
pub(super) struct StopSequences {
    patterns: Vec<Pattern>,
    pending: String,
    head: usize,
    part: Option<(usize, usize)>,
    output_index: Option<usize>,
    matched: Option<String>,
}

#[derive(Clone)]
struct Pattern {
    text: String,
    fallback: Vec<usize>,
    position: usize,
}

impl StopSequences {
    pub fn new(stops: &[String]) -> Self {
        Self {
            patterns: stops
                .iter()
                .filter(|s| !s.is_empty())
                .map(|text| {
                    let bytes = text.as_bytes();
                    let mut fallback = vec![0; bytes.len()];
                    for i in 1..bytes.len() {
                        let mut j = fallback[i - 1];
                        while j > 0 && bytes[i] != bytes[j] {
                            j = fallback[j - 1];
                        }
                        if bytes[i] == bytes[j] {
                            j += 1;
                        }
                        fallback[i] = j;
                    }
                    Pattern {
                        text: text.clone(),
                        fallback,
                        position: 0,
                    }
                })
                .collect(),
            ..Self::default()
        }
    }

    pub fn matched(&self) -> Option<&str> {
        self.matched.as_deref()
    }

    pub fn has_pending(&self) -> bool {
        self.pending.len() > self.head
    }

    pub fn flush_item(&mut self, output_index: Option<usize>) -> Option<TextDelta> {
        if output_index.is_some() && output_index != self.output_index {
            return None;
        }
        self.flush()
    }

    pub fn flush(&mut self) -> Option<TextDelta> {
        for pattern in &mut self.patterns {
            pattern.position = 0;
        }
        self.output_index = None;
        let (index, content_index) = self.part.take()?;
        let text = self.pending[self.head..].to_owned();
        self.pending.clear();
        self.head = 0;
        (!text.is_empty()).then_some(TextDelta {
            index,
            content_index,
            start: false,
            text,
            refusal: false,
        })
    }

    pub fn accept(&mut self, mut delta: TextDelta, output_index: usize) -> [Option<TextDelta>; 2] {
        if self.matched.is_some() {
            return [None, None];
        }
        if self.patterns.is_empty() {
            return [Some(delta), None];
        }
        let pending = if delta.refusal || self.part != Some((delta.index, delta.content_index)) {
            self.flush()
        } else {
            None
        };
        if delta.refusal {
            return [pending, Some(delta)];
        }
        self.part = Some((delta.index, delta.content_index));
        self.output_index = Some(output_index);
        let mut text = String::new();
        for ch in delta.text.chars() {
            self.pending.push(ch);
            let mut matched = None;
            let mut encoded = [0; 4];
            for byte in ch.encode_utf8(&mut encoded).bytes() {
                for (index, pattern) in self.patterns.iter_mut().enumerate() {
                    let bytes = pattern.text.as_bytes();
                    while pattern.position > 0 && bytes[pattern.position] != byte {
                        pattern.position = pattern.fallback[pattern.position - 1];
                    }
                    if bytes[pattern.position] == byte {
                        pattern.position += 1;
                    }
                    if pattern.position == bytes.len() && matched.is_none() {
                        matched = Some(index);
                    }
                }
            }
            if let Some(index) = matched {
                let stop = &self.patterns[index].text;
                text.push_str(&self.pending[self.head..self.pending.len() - stop.len()]);
                self.pending.clear();
                self.head = 0;
                self.matched = Some(stop.clone());
                break;
            }
            let keep = self.patterns.iter().map(|p| p.position).max().unwrap_or(0);
            let safe = self.pending.len() - keep;
            text.push_str(&self.pending[self.head..safe]);
            self.head = safe;
            // Amortized compaction: no per-character allocation or prefix copy.
            if self.head >= MAX_STOP_BYTES {
                self.pending.drain(..self.head);
                self.head = 0;
            }
        }
        delta.text = text;
        [pending, Some(delta)]
    }
}

pub(super) fn text_boundary(payload: &serde_json::Value) -> bool {
    let kind = payload["type"].as_str().unwrap_or("");
    kind.starts_with("response.reasoning_")
        || kind.starts_with("response.function_call_")
        || kind.starts_with("response.web_search_call.")
        || (matches!(
            kind,
            "response.output_item.added" | "response.output_item.done"
        ) && payload["item"]["type"] != "message")
}
