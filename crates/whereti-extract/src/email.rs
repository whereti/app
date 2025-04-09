//! E-mail as text: `.eml` (MIME, via `mail-parser`) and Outlook `.msg`
//! (see `ole::msg`). The text starts with the headers people search for —
//! subject, sender, recipients, date — then the body (plain text, or the
//! HTML part turned into text) and the attachment names.

use mail_parser::{MessageParser, MimeHeaders};

use crate::{html_to_text, ExtractError, Out};

pub(crate) fn eml(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let msg = MessageParser::default().parse(bytes).ok_or_else(|| ExtractError::Damaged("not an e-mail".into()))?;
    let people = |a: Option<&mail_parser::Address>| -> String {
        a.map(|a| {
            a.iter()
                .map(|p| match (p.name(), p.address()) {
                    (Some(n), Some(e)) => format!("{n} <{e}>"),
                    (Some(n), None) => n.to_string(),
                    (None, Some(e)) => e.to_string(),
                    (None, None) => String::new(),
                })
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
    };
    let headers = Headers {
        subject: msg.subject().unwrap_or("").to_string(),
        from: people(msg.from()),
        to: people(msg.to()),
        cc: people(msg.cc()),
        date: msg.date().map(|d| d.to_rfc3339().replace('T', " ")).unwrap_or_default(),
    };
    headers.write(out);

    // the body: plain text parts, else the HTML ones as text
    let mut any = false;
    for i in 0..msg.text_body_count() {
        if let Some(t) = msg.body_text(i) {
            out.push(&t);
            out.newline();
            any = true;
        }
    }
    if !any {
        for i in 0..msg.html_body_count() {
            if let Some(h) = msg.body_html(i) {
                out.push(&html_to_text(&h));
                out.newline();
            }
        }
    }
    let names: Vec<String> = msg.attachments().filter_map(|a| a.attachment_name()).map(str::to_string).collect();
    attachments(&names, out);
    Ok(())
}

/// The e-mail headers, written the same way for .eml and .msg.
#[derive(Default)]
pub(crate) struct Headers {
    pub subject: String,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub date: String,
}

impl Headers {
    pub(crate) fn write(&self, out: &mut Out) {
        if !self.subject.is_empty() {
            out.push(&self.subject);
            out.newline();
        }
        for (label, v) in [("From", &self.from), ("To", &self.to), ("Cc", &self.cc), ("Date", &self.date)] {
            if !v.is_empty() {
                out.push(&format!("{label}: {v}"));
                out.newline();
            }
        }
        out.push("\n");
    }
}

pub(crate) fn attachments(names: &[String], out: &mut Out) {
    if !names.is_empty() {
        out.newline();
        out.push(&format!("📎 {}", names.join(", ")));
        out.newline();
    }
}
