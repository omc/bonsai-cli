//! The few HTML pages the mock serves in place of the dashboard (the login
//! approval page and the checkout page): plain forms, no JavaScript.
//!
//! Everything a client sent, and everything from a URL, goes through
//! [`escape`] before it lands in a page: the approval page shows a hostname
//! and OS that anyone starting a login chooses.

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

/// `s` with the five HTML-significant characters escaped, safe in text and
/// in a double- or single-quoted attribute.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A complete page. `title` is escaped; `body` is trusted HTML, which the
/// caller has built with [`escape`].
pub fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{} - bonsai-mock</title>\n\
         <style>body{{font-family:system-ui,sans-serif;max-width:40rem;margin:2rem auto;padding:0 1rem;line-height:1.5}}\
         .code{{font:600 2rem ui-monospace,monospace;letter-spacing:.1em}}\
         .warn{{border-left:4px solid #c60;padding:.5rem 1rem;background:#fff4e5}}\
         fieldset{{margin:1rem 0}}th{{text-align:left;padding-right:1rem}}</style>\n\
         </head>\n<body>\n<h1>{}</h1>\n{body}\n</body>\n</html>\n",
        escape(title),
        escape(title),
    );
    (status, Html(html)).into_response()
}

/// A page with one paragraph of plain text.
pub fn message(status: StatusCode, title: &str, text: &str) -> Response {
    page(status, title, &format!("<p>{}</p>", escape(text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_and_quotes() {
        assert_eq!(
            escape(r#"<script>alert("x" & 'y')</script>"#),
            "&lt;script&gt;alert(&quot;x&quot; &amp; &#39;y&#39;)&lt;/script&gt;"
        );
        assert_eq!(escape("plain-host_1.local"), "plain-host_1.local");
    }
}
