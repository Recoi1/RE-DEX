//! Outgoing mail: SMTP, or (dev and tests) one `.eml` file per mail in a folder.
use crate::config::MailConfig;
use lettre::message::Mailbox;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use std::path::PathBuf;

pub enum Mailer {
    Dir(PathBuf),
    Smtp(AsyncSmtpTransport<Tokio1Executor>, Mailbox),
}

impl Mailer {
    pub fn new(cfg: &MailConfig, from: &str) -> Result<Mailer, String> {
        match cfg {
            MailConfig::Dir(d) => {
                std::fs::create_dir_all(d).map_err(|e| format!("REDEX_MAIL_DIR {}: {e}", d.display()))?;
                Ok(Mailer::Dir(d.clone()))
            }
            MailConfig::Smtp(url) => {
                // mail carries login codes: never in the clear
                if url.starts_with("smtp://") && !url.contains("tls=required") {
                    return Err("REDEX_SMTP_URL: use smtps:// or smtp://...?tls=required (no plain SMTP)".into());
                }
                let t = AsyncSmtpTransport::<Tokio1Executor>::from_url(url).map_err(|e| format!("REDEX_SMTP_URL: {e}"))?.build();
                Ok(Mailer::Smtp(t, from.parse().map_err(|e| format!("REDEX_MAIL_FROM: {e}"))?))
            }
        }
    }

    pub async fn send(&self, to: &str, subject: &str, body: String) -> Result<(), String> {
        match self {
            Mailer::Dir(d) => {
                let mut r = [0u8; 8];
                getrandom::fill(&mut r).map_err(|e| e.to_string())?;
                let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
                let name = format!("{ms}-{:016x}.eml", u64::from_le_bytes(r));
                let text = format!("To: {to}\r\nSubject: {subject}\r\n\r\n{body}\r\n");
                tokio::fs::write(d.join(name), text).await.map_err(|e| e.to_string())
            }
            Mailer::Smtp(t, from) => {
                let to: Mailbox = to.parse().map_err(|e| format!("bad recipient: {e}"))?;
                let m = Message::builder().from(from.clone()).to(to).subject(subject).body(body).map_err(|e| e.to_string())?;
                t.send(m).await.map(drop).map_err(|e| e.to_string())
            }
        }
    }
}
