//! Complexity classifier — a Rust port of litellm's `router_strategy/complexity_router`.
//!
//! Rule-based, local, sub-millisecond, zero API calls. Scores the request across weighted
//! dimensions (code/reasoning/technical keywords, length, multi-step structure) and maps the sum to
//! one of four tiers. On top of the faithful port we fold in three Claude Code-specific signals
//! (see `classify`): the Anthropic `thinking` field, the tool count, and the conversation context
//! size — so terse agentic turns ("fix it") aren't mis-classified as trivial.

use crate::settings::Settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Simple,
    Medium,
    Complex,
    Reasoning,
}

impl Tier {
    /// Resolve the configured model for this tier (caller guarantees `routing_enabled`).
    pub fn model<'a>(&self, s: &'a Settings) -> Option<&'a str> {
        match self {
            Tier::Simple => s.simple_model.as_deref(),
            Tier::Medium => s.medium_model.as_deref(),
            Tier::Complex => s.complex_model.as_deref(),
            Tier::Reasoning => s.reasoning_model.as_deref(),
        }
    }
}

// ─── Keyword lists (verbatim from complexity_router/config.py) ───

const CODE_KEYWORDS: &[&str] = &[
    "function", "class", "def", "const", "let", "var", "import", "export", "return", "async",
    "await", "try", "catch", "exception", "error", "debug", "api", "endpoint", "request",
    "response", "database", "sql", "query", "schema", "algorithm", "implement", "refactor",
    "optimize", "python", "javascript", "typescript", "java", "rust", "golang", "react", "vue",
    "angular", "node", "docker", "kubernetes", "git", "commit", "merge", "branch", "pull request",
];

const REASONING_KEYWORDS: &[&str] = &[
    "step by step", "think through", "let's think", "reason through", "analyze this", "break down",
    "explain your reasoning", "show your work", "chain of thought", "think carefully",
    "consider all", "evaluate", "pros and cons", "compare and contrast", "weigh the options",
    "logical", "deduce", "infer", "conclude",
];

const TECHNICAL_KEYWORDS: &[&str] = &[
    "architecture", "distributed", "scalable", "microservice", "machine learning", "neural network",
    "deep learning", "encryption", "authentication", "authorization", "performance", "latency",
    "throughput", "benchmark", "concurrency", "parallel", "threading", "memory", "cpu", "gpu",
    "optimization", "protocol", "tcp", "http", "grpc", "websocket", "container", "orchestration",
];

const SIMPLE_KEYWORDS: &[&str] = &[
    "what is", "what's", "define", "definition of", "who is", "who was", "when did", "when was",
    "where is", "where was", "how many", "how much", "yes or no", "true or false", "simple",
    "brief", "short", "quick", "hello", "hi", "hey", "thanks", "thank you", "goodbye", "bye",
    "okay",
];

// ─── Weights / boundaries / thresholds (verbatim from config.py) ───

const W_TOKEN: f64 = 0.10;
const W_CODE: f64 = 0.30;
const W_REASONING: f64 = 0.25;
const W_TECHNICAL: f64 = 0.25;
const W_SIMPLE: f64 = 0.05;
const W_MULTISTEP: f64 = 0.03;
const W_QUESTION: f64 = 0.02;

const B_SIMPLE_MEDIUM: f64 = 0.15;
const B_MEDIUM_COMPLEX: f64 = 0.35;
const B_COMPLEX_REASONING: f64 = 0.60;

const TOK_SIMPLE: usize = 15;
const TOK_COMPLEX: usize = 400;

/// Max upward nudge from Claude Code's tool list. Agentic sessions always carry tools, so this
/// gently biases real coding sessions away from the SIMPLE tier without ever dominating the score.
const MAX_TOOL_BOOST: f64 = 0.10;

/// `text.len() / 4` — same heuristic as `_estimate_tokens`.
fn estimate_tokens(chars: usize) -> usize {
    chars / 4
}

/// Strip Claude Code's injected `<system-reminder>…</system-reminder>` blocks from a turn so the
/// classifier scores the user's actual ask, not the harness boilerplate that CC prepends to the
/// user message. An unclosed opener drops the remainder.
pub fn strip_noise(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Word-boundary keyword match. Single words match on non-alphanumeric boundaries (so "api" does
/// not match "capital"); multi-word phrases use substring matching. `text` must be lowercase.
fn keyword_matches(text: &str, keyword: &str) -> bool {
    if keyword.contains(' ') {
        return text.contains(keyword);
    }
    let kb = keyword.as_bytes();
    let tb = text.as_bytes();
    if kb.is_empty() || tb.len() < kb.len() {
        return false;
    }
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while i + kb.len() <= tb.len() {
        if &tb[i..i + kb.len()] == kb {
            let before_ok = i == 0 || !is_word(tb[i - 1]);
            let after = i + kb.len();
            let after_ok = after == tb.len() || !is_word(tb[after]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn count_matches(text: &str, keywords: &[&str]) -> usize {
    keywords.iter().filter(|k| keyword_matches(text, k)).count()
}

/// `(count >= high) -> high_score; (count >= low) -> low_score; else none_score`.
fn tiered_score(count: usize, low: usize, high: usize, low_s: f64, high_s: f64, none_s: f64) -> f64 {
    if count >= high {
        high_s
    } else if count >= low {
        low_s
    } else {
        none_s
    }
}

fn score_token_count(estimated_tokens: usize) -> f64 {
    if estimated_tokens < TOK_SIMPLE {
        -1.0
    } else if estimated_tokens > TOK_COMPLEX {
        1.0
    } else {
        0.0
    }
}

/// Port of the four multi-step regexes: `first…then`, `step\d`, `\d+. `, `[a-z]) `.
fn has_multistep(text: &str) -> bool {
    let bytes = text.as_bytes();
    // first … then
    if let Some(f) = text.find("first") {
        if text[f..].contains("then") {
            return true;
        }
    }
    // "step" optionally followed by whitespace then a digit
    if let Some(s) = text.find("step") {
        let rest = text[s + 4..].trim_start();
        if rest.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
            return true;
        }
    }
    // digit(s) then "." then whitespace ; or letter then ")" then whitespace
    for i in 0..bytes.len() {
        // \d+\.\s
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j + 1 < bytes.len() && bytes[j] == b'.' && bytes[j + 1].is_ascii_whitespace() {
                return true;
            }
        }
        // [a-z]\)\s
        if bytes[i].is_ascii_alphabetic()
            && i + 2 < bytes.len()
            && bytes[i + 1] == b')'
            && bytes[i + 2].is_ascii_whitespace()
        {
            return true;
        }
    }
    false
}

/// Classify a request into a complexity tier, scoring **the user message only**.
///
/// Unlike the litellm reference (which folds the system prompt into code/technical scoring for
/// "deployment context"), we deliberately ignore Claude Code's system prompt: it is a ~7 KB tool
/// catalog saturated with code/technical keywords that would push every turn to COMPLEX. The user's
/// actual ask is the routing signal. `<system-reminder>` blocks CC injects into the user turn are
/// stripped first (`strip_noise`).
///
/// Claude Code signals layered on top of the port:
/// - `tool_count`: number of tools advertised → small upward boost (`MAX_TOOL_BOOST`).
/// - `context_chars`: total (de-noised) conversation text → drives the token-count dimension, so a
///   terse turn atop a large context still scores up.
///
/// Note: we deliberately do NOT use the Anthropic `thinking` field. In Claude Code thinking is a
/// global on/off mode (set for every request), not a per-turn complexity signal — keying off it
/// would force every turn to the reasoning tier and defeat routing.
pub fn classify(user_msg: &str, tool_count: usize, context_chars: usize) -> Tier {
    let user_text = strip_noise(user_msg).to_lowercase();

    let estimated_tokens = estimate_tokens(context_chars.max(user_text.len()));

    let code = tiered_score(count_matches(&user_text, CODE_KEYWORDS), 1, 2, 0.5, 1.0, 0.0);
    let reasoning_count = count_matches(&user_text, REASONING_KEYWORDS);
    let reasoning = tiered_score(reasoning_count, 1, 2, 0.7, 1.0, 0.0);
    let technical = tiered_score(count_matches(&user_text, TECHNICAL_KEYWORDS), 2, 4, 0.5, 1.0, 0.0);
    // simpleIndicators: any match drives the (negative-weighted) dimension to -1.0.
    let simple = tiered_score(count_matches(&user_text, SIMPLE_KEYWORDS), 1, 2, -1.0, -1.0, 0.0);
    let multistep = if has_multistep(&user_text) { 0.5 } else { 0.0 };
    let question = if user_text.matches('?').count() > 3 { 0.5 } else { 0.0 };

    let mut score = score_token_count(estimated_tokens) * W_TOKEN
        + code * W_CODE
        + reasoning * W_REASONING
        + technical * W_TECHNICAL
        + simple * W_SIMPLE
        + multistep * W_MULTISTEP
        + question * W_QUESTION;

    // CC signal: nudge agentic (tool-bearing) sessions up, capped so it can't dominate.
    score += (tool_count as f64 * 0.01).min(MAX_TOOL_BOOST);

    // Reasoning override: 2+ explicit reasoning markers in the user message.
    if reasoning_count >= 2 {
        return Tier::Reasoning;
    }

    if score < B_SIMPLE_MEDIUM {
        Tier::Simple
    } else if score < B_MEDIUM_COMPLEX {
        Tier::Medium
    } else if score < B_COMPLEX_REASONING {
        Tier::Complex
    } else {
        Tier::Reasoning
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(user: &str) -> Tier {
        classify(user, 0, user.len())
    }

    #[test]
    fn simple_question() {
        assert_eq!(c("What is 2+2?"), Tier::Simple);
    }

    #[test]
    fn greeting() {
        assert_eq!(c("hello"), Tier::Simple);
    }

    #[test]
    fn two_reasoning_markers_override() {
        // "step by step" + "analyze this" + "show your work" = 3 markers → REASONING regardless.
        let t = c("Let's think step by step, analyze this, and show your work.");
        assert_eq!(t, Tier::Reasoning);
    }

    #[test]
    fn code_heavy_is_not_simple() {
        let t = c("Implement an async function that runs a database query and returns a \
                   response, then refactor the api endpoint and optimize the algorithm.");
        assert!(matches!(t, Tier::Complex | Tier::Reasoning | Tier::Medium));
        assert_ne!(t, Tier::Simple);
    }

    #[test]
    fn tool_boost_lifts_borderline_session() {
        // A borderline request (2 technical terms → base 0.125, just under MEDIUM) stays SIMPLE on
        // its own, but a tool-bearing agentic session pushes it up a tier.
        let msg =
            "Please review the latency and the memory characteristics of this running system.";
        assert_eq!(classify(msg, 0, msg.len()), Tier::Simple);
        assert_eq!(classify(msg, 15, msg.len()), Tier::Medium);
    }

    #[test]
    fn strips_system_reminder_noise() {
        // The real ask ("hi") is buried after a CC system-reminder block → must classify SIMPLE,
        // not be dragged up by reminder/tool-catalog text.
        let polluted = "<system-reminder>\nToday's date is 2026-06-23. You can use these tools: \
            function, class, api, database, query, endpoint, async.\n</system-reminder>\nhi";
        assert_eq!(classify(polluted, 27, polluted.len()), Tier::Simple);
        assert_eq!(strip_noise(polluted), "hi");
    }

    #[test]
    fn keyword_boundary_no_false_positive() {
        assert!(!keyword_matches("the capital of france", "api"));
        assert!(keyword_matches("call the api now", "api"));
    }
}
