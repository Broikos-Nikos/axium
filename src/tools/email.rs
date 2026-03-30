use anyhow::{Context, Result};
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub from: String,
}

impl SmtpConfig {
    pub fn is_configured(&self) -> bool {
        !self.host.is_empty() && !self.user.is_empty() && !self.password.is_empty() && !self.from.is_empty()
    }
}

pub async fn send_email(
    config: &SmtpConfig,
    to: &str,
    subject: &str,
    body: &str,
    html: bool,
) -> Result<String> {
    if !config.is_configured() {
        anyhow::bail!("SMTP is not configured. Set smtp_host, smtp_user, smtp_password, and smtp_from in Settings.");
    }

    // Validate recipient
    if to.is_empty() || !to.contains('@') {
        anyhow::bail!("Invalid recipient address: {}", to);
    }

    let content_type = if html {
        ContentType::TEXT_HTML
    } else {
        ContentType::TEXT_PLAIN
    };

    let email = Message::builder()
        .from(config.from.parse().context("Invalid 'from' address in SMTP settings")?)
        .to(to.parse().context(format!("Invalid recipient address: {}", to))?)
        .subject(subject)
        .header(content_type)
        .body(body.to_string())
        .context("Failed to build email message")?;

    let creds = Credentials::new(config.user.clone(), config.password.clone());

    // Port 465 = implicit TLS (SMTPS), port 587 = STARTTLS
    let mailer = if config.port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host)
            .context(format!("Invalid SMTP host: {}", config.host))?
            .port(config.port)
            .credentials(creds)
            .build()
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host)
            .context(format!("Invalid SMTP host: {}", config.host))?
            .port(config.port)
            .credentials(creds)
            .build()
    };

    mailer.send(email).await.context("Failed to send email")?;

    Ok(format!("Email sent to {} — subject: {}", to, subject))
}
