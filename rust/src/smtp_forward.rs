//! Bounded SMTP submission for opt-in SMS forwarding. Credentials are sent
//! only after certificate-validated TLS, with DNS answers pinned before dial.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::TlsConnector;
use zeroize::{Zeroize, Zeroizing};

use crate::sms_forward::public_ip;

const MAX_REPLY_LINES: usize = 24;
const MAX_REPLY_LINE: usize = 512;
const MAX_RESOLVED: usize = 16;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub to: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    pub to: String,
}

impl Drop for Settings {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

impl Settings {
    pub fn apply(&mut self, change: Update) {
        self.host = change.host;
        self.port = change.port;
        self.username = change.username;
        if let Some(password) = change.password {
            self.password.zeroize();
            self.password = password;
        }
        self.to = change.to;
    }

    pub fn configured(&self) -> bool {
        valid_host(&self.host)
            && matches!(self.port, 465 | 587)
            && valid_mailbox(&self.username)
            && !self.password.is_empty()
            && self.password.len() <= 512
            && !self.password.chars().any(char::is_control)
            && valid_mailbox(&self.to)
    }

    pub fn safe(&self) -> bool {
        self.configured()
            || self.host.is_empty()
                && self.port == 0
                && self.username.is_empty()
                && self.password.is_empty()
                && self.to.is_empty()
    }
}

fn valid_host(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    host.len() <= 253
        && host.is_ascii()
        && host.contains('.')
        && host.parse::<IpAddr>().is_err()
        && !host.ends_with('.')
        && !lower.ends_with(".local")
        && !lower.ends_with(".internal")
        && !lower.ends_with(".localhost")
        && host.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn valid_mailbox(value: &str) -> bool {
    if value.len() > 254 || !value.is_ascii() {
        return false;
    }
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    (1..=64).contains(&local.len())
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".!#$%&'*+/=?^_`{|}~-".contains(&byte))
        && valid_host(domain)
}

async fn read_reply<S: AsyncRead + Unpin>(stream: &mut S) -> Result<(u16, Vec<String>), String> {
    let mut result = Vec::new();
    let mut expected = None;
    for _ in 0..MAX_REPLY_LINES {
        let mut line = Vec::new();
        loop {
            if line.len() >= MAX_REPLY_LINE {
                return Err("delivery_failed".into());
            }
            let byte = stream.read_u8().await.map_err(|_| "delivery_failed")?;
            line.push(byte);
            if byte == b'\n' {
                break;
            }
        }
        if line.len() < 6 || !line.ends_with(b"\r\n") || !line[..3].iter().all(u8::is_ascii_digit) {
            return Err("delivery_failed".into());
        }
        let code = std::str::from_utf8(&line[..3])
            .map_err(|_| "delivery_failed")?
            .parse::<u16>()
            .map_err(|_| "delivery_failed")?;
        if expected.is_some_and(|old| old != code) || !matches!(line[3], b'-' | b' ') {
            return Err("delivery_failed".into());
        }
        expected = Some(code);
        result.push(String::from_utf8_lossy(&line[4..line.len() - 2]).into_owned());
        if line[3] == b' ' {
            return Ok((code, result));
        }
    }
    Err("delivery_failed".into())
}

async fn command<S: AsyncWrite + Unpin>(stream: &mut S, line: &str) -> Result<(), String> {
    stream
        .write_all(line.as_bytes())
        .await
        .map_err(|_| "delivery_failed")?;
    stream
        .write_all(b"\r\n")
        .await
        .map_err(|_| "delivery_failed".to_string())
}

async fn expect<S: AsyncRead + Unpin>(stream: &mut S, code: u16) -> Result<Vec<String>, String> {
    let (actual, lines) = read_reply(stream).await?;
    if actual != code {
        return Err("delivery_failed".into());
    }
    Ok(lines)
}

async fn ehlo<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> Result<Vec<String>, String> {
    command(stream, "EHLO zwrt-datad").await?;
    expect(stream, 250).await
}

fn advertises(lines: &[String], capability: &str) -> bool {
    lines.iter().any(|line| {
        line.split_ascii_whitespace()
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(capability))
    })
}

async fn submit<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    settings: &Settings,
    subject: &str,
    body: &str,
    greeting: bool,
) -> Result<(), String> {
    if greeting {
        expect(stream, 220).await?;
    }
    let capabilities = ehlo(stream).await?;
    if !capabilities.iter().any(|line| {
        line.split_ascii_whitespace()
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("AUTH"))
            && line
                .split_ascii_whitespace()
                .skip(1)
                .any(|name| name.eq_ignore_ascii_case("PLAIN"))
    }) {
        return Err("delivery_failed".into());
    }
    let credential = Zeroizing::new(format!("\0{}\0{}", settings.username, settings.password));
    let encoded = Zeroizing::new(STANDARD.encode(credential.as_bytes()));
    let auth_command = Zeroizing::new(format!("AUTH PLAIN {}", encoded.as_str()));
    command(stream, &auth_command).await?;
    let (code, _) = read_reply(stream).await?;
    if code != 235 {
        return Err("smtp_auth_failed".into());
    }
    command(stream, &format!("MAIL FROM:<{}>", settings.username)).await?;
    expect(stream, 250).await?;
    command(stream, &format!("RCPT TO:<{}>", settings.to)).await?;
    expect(stream, 250).await?;
    command(stream, "DATA").await?;
    expect(stream, 354).await?;

    let subject = STANDARD.encode(subject.as_bytes());
    let encoded_body = STANDARD.encode(body.as_bytes());
    let headers = format!(
        "From: <{}>\r\nTo: <{}>\r\nSubject: =?UTF-8?B?{subject}?=\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=UTF-8\r\nContent-Transfer-Encoding: base64\r\n\r\n",
        settings.username, settings.to
    );
    stream
        .write_all(headers.as_bytes())
        .await
        .map_err(|_| "delivery_failed")?;
    for chunk in encoded_body.as_bytes().chunks(76) {
        stream
            .write_all(chunk)
            .await
            .map_err(|_| "delivery_failed")?;
        stream
            .write_all(b"\r\n")
            .await
            .map_err(|_| "delivery_failed")?;
    }
    stream
        .write_all(b".\r\n")
        .await
        .map_err(|_| "delivery_failed")?;
    expect(stream, 250).await?;
    let _ = command(stream, "QUIT").await;
    Ok(())
}

fn tls_connector(roots: RootCertStore) -> TlsConnector {
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

async fn send_to(
    settings: &Settings,
    socket: TcpStream,
    roots: RootCertStore,
    subject: &str,
    body: &str,
) -> Result<(), String> {
    let name = ServerName::try_from(settings.host.clone()).map_err(|_| "delivery_failed")?;
    let connector = tls_connector(roots);
    if settings.port == 465 {
        let mut stream = connector
            .connect(name, socket)
            .await
            .map_err(|_| "delivery_failed")?;
        return submit(&mut stream, settings, subject, body, true).await;
    }
    let mut socket = socket;
    expect(&mut socket, 220).await?;
    let capabilities = ehlo(&mut socket).await?;
    if !advertises(&capabilities, "STARTTLS") {
        return Err("delivery_failed".into());
    }
    command(&mut socket, "STARTTLS").await?;
    expect(&mut socket, 220).await?;
    // RFC 3207: forget the plaintext EHLO result after upgrading.
    let mut stream = connector
        .connect(name, socket)
        .await
        .map_err(|_| "delivery_failed")?;
    submit(&mut stream, settings, subject, body, false).await
}

pub async fn send(settings: &Settings, subject: &str, body: &str) -> Result<(), String> {
    if !settings.configured()
        || subject.len() > 64
        || subject.chars().any(char::is_control)
        || body.len() > 4096
    {
        return Err("invalid_forward_config".into());
    }
    timeout(Duration::from_secs(20), async {
        let lookup = timeout(
            Duration::from_secs(4),
            tokio::net::lookup_host((settings.host.as_str(), settings.port)),
        )
        .await
        .map_err(|_| "delivery_failed")?
        .map_err(|_| "delivery_failed")?;
        let addresses: Vec<SocketAddr> = lookup.collect();
        if addresses.is_empty()
            || addresses.len() > MAX_RESOLVED
            || addresses.iter().any(|address| !public_ip(address.ip()))
        {
            return Err("invalid_forward_config".into());
        }
        let mut connected = None;
        for address in addresses {
            if let Ok(Ok(socket)) =
                timeout(Duration::from_secs(4), TcpStream::connect(address)).await
            {
                connected = Some(socket);
                break;
            }
        }
        let socket = connected.ok_or("delivery_failed")?;
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        send_to(settings, socket, roots, subject, body).await
    })
    .await
    .map_err(|_| "delivery_failed".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::{ServerConfig, pki_types::PrivateKeyDer};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    #[test]
    fn destinations_are_bounded_and_never_allow_header_injection() {
        let mut settings = Settings {
            host: "smtp.example.com".into(),
            port: 465,
            username: "sender@example.com".into(),
            password: "app-password".into(),
            to: "recipient@example.net".into(),
        };
        assert!(settings.configured());
        settings.host = "127.0.0.1".into();
        assert!(!settings.safe());
        settings.host = "smtp.example.com".into();
        settings.to = "victim@example.net\r\nBcc: spy@example.net".into();
        assert!(!settings.safe());
        settings.to = "recipient@example.net".into();
        settings.port = 25;
        assert!(!settings.safe());
        assert!(Settings::default().safe());
    }

    async fn server_line<S: AsyncRead + Unpin>(stream: &mut S) -> String {
        let mut line = Vec::new();
        loop {
            let byte = stream.read_u8().await.unwrap();
            line.push(byte);
            assert!(line.len() < 8192);
            if byte == b'\n' {
                return String::from_utf8(line).unwrap();
            }
        }
    }

    async fn server_submit<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) {
        assert_eq!(server_line(stream).await, "EHLO zwrt-datad\r\n");
        stream
            .write_all(b"250-fixture\r\n250 AUTH PLAIN\r\n")
            .await
            .unwrap();
        let auth = server_line(stream).await;
        let encoded = auth.strip_prefix("AUTH PLAIN ").unwrap().trim_end();
        assert_eq!(
            STANDARD.decode(encoded).unwrap(),
            b"\0sender@example.com\0app-password"
        );
        stream.write_all(b"235 accepted\r\n").await.unwrap();
        assert_eq!(
            server_line(stream).await,
            "MAIL FROM:<sender@example.com>\r\n"
        );
        stream.write_all(b"250 sender ok\r\n").await.unwrap();
        assert_eq!(
            server_line(stream).await,
            "RCPT TO:<recipient@example.net>\r\n"
        );
        stream.write_all(b"250 recipient ok\r\n").await.unwrap();
        assert_eq!(server_line(stream).await, "DATA\r\n");
        stream.write_all(b"354 send data\r\n").await.unwrap();
        let mut headers = Vec::new();
        loop {
            let line = server_line(stream).await;
            if line == "\r\n" {
                break;
            }
            headers.push(line);
        }
        assert!(
            headers
                .iter()
                .any(|line| line == "Content-Transfer-Encoding: base64\r\n")
        );
        let mut encoded_body = String::new();
        loop {
            let line = server_line(stream).await;
            if line == ".\r\n" {
                break;
            }
            encoded_body.push_str(line.trim_end());
        }
        assert_eq!(
            String::from_utf8(STANDARD.decode(encoded_body).unwrap()).unwrap(),
            "来自 10086:\n合成测试短信"
        );
        stream.write_all(b"250 queued\r\n").await.unwrap();
    }

    #[tokio::test]
    async fn both_tls_modes_authenticate_and_submit_only_inside_tls() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        for port in [465, 587] {
            let certified =
                rcgen::generate_simple_self_signed(vec!["smtp.example.com".into()]).unwrap();
            let mut roots = RootCertStore::empty();
            roots.add(certified.cert.der().clone()).unwrap();
            let server = ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![certified.cert.der().clone()],
                    PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into()),
                )
                .unwrap();
            let acceptor = TlsAcceptor::from(Arc::new(server));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server_task = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                if port == 587 {
                    let mut plain = socket;
                    plain.write_all(b"220 fixture\r\n").await.unwrap();
                    assert_eq!(server_line(&mut plain).await, "EHLO zwrt-datad\r\n");
                    plain
                        .write_all(b"250-fixture\r\n250 STARTTLS\r\n")
                        .await
                        .unwrap();
                    assert_eq!(server_line(&mut plain).await, "STARTTLS\r\n");
                    plain.write_all(b"220 go ahead\r\n").await.unwrap();
                    let mut tls = acceptor.accept(plain).await.unwrap();
                    server_submit(&mut tls).await;
                } else {
                    let mut tls = acceptor.accept(socket).await.unwrap();
                    tls.write_all(b"220 fixture\r\n").await.unwrap();
                    server_submit(&mut tls).await;
                }
            });
            let settings = Settings {
                host: "smtp.example.com".into(),
                port,
                username: "sender@example.com".into(),
                password: "app-password".into(),
                to: "recipient@example.net".into(),
            };
            let socket = TcpStream::connect(address).await.unwrap();
            timeout(
                Duration::from_secs(5),
                send_to(
                    &settings,
                    socket,
                    roots,
                    "新短信通知",
                    "来自 10086:\n合成测试短信",
                ),
            )
            .await
            .unwrap()
            .unwrap();
            server_task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn refuses_starttls_downgrade_before_sending_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"220 fixture\r\n").await.unwrap();
            assert_eq!(server_line(&mut socket).await, "EHLO zwrt-datad\r\n");
            socket.write_all(b"250 AUTH PLAIN\r\n").await.unwrap();
            let mut buffer = [0u8; 1];
            assert_eq!(socket.read(&mut buffer).await.unwrap(), 0);
        });
        let settings = Settings {
            host: "smtp.example.com".into(),
            port: 587,
            username: "sender@example.com".into(),
            password: "fixture-app-password".into(),
            to: "recipient@example.net".into(),
        };
        let socket = TcpStream::connect(address).await.unwrap();
        assert!(
            send_to(
                &settings,
                socket,
                RootCertStore::empty(),
                "新短信通知",
                "synthetic"
            )
            .await
            .is_err()
        );
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_a_valid_but_wrong_tls_hostname() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let certified =
            rcgen::generate_simple_self_signed(vec!["wrong.example.com".into()]).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(certified.cert.der().clone()).unwrap();
        let server = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![certified.cert.der().clone()],
                PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into()),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            assert!(acceptor.accept(socket).await.is_err());
        });
        let settings = Settings {
            host: "smtp.example.com".into(),
            port: 465,
            username: "sender@example.com".into(),
            password: "fixture-app-password".into(),
            to: "recipient@example.net".into(),
        };
        let socket = TcpStream::connect(address).await.unwrap();
        assert!(
            send_to(&settings, socket, roots, "新短信通知", "synthetic")
                .await
                .is_err()
        );
        server_task.await.unwrap();
    }
}
