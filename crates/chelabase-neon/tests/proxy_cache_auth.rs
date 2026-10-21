//! RT1 T1 (R11, P5, S1, S6): the control plane's Valkey publisher authenticates as its ACL user
//! (`control`), with the password exactly as read from its file. A fake Redis-protocol server
//! records each command and answers it.

use chelabase_neon::proxy_cache::{CHANNEL, ProxyCache};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

type Command = Vec<String>;

async fn fake_valkey() -> (String, mpsc::UnboundedReceiver<Command>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("redis://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let (read, mut write) = socket.into_split();
                let mut read = BufReader::new(read);
                loop {
                    let mut header = String::new();
                    if read.read_line(&mut header).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let Some(n) = header
                        .trim_end()
                        .strip_prefix('*')
                        .and_then(|n| n.parse::<usize>().ok())
                    else {
                        return;
                    };
                    let mut cmd = Vec::with_capacity(n);
                    for _ in 0..n {
                        let mut len = String::new();
                        read.read_line(&mut len).await.unwrap();
                        let len: usize = len.trim_end().trim_start_matches('$').parse().unwrap();
                        let mut buf = vec![0u8; len + 2];
                        read.read_exact(&mut buf).await.unwrap();
                        buf.truncate(len);
                        cmd.push(String::from_utf8_lossy(&buf).into_owned());
                    }
                    let reply: &[u8] = match cmd[0].to_ascii_uppercase().as_str() {
                        "PUBLISH" => b":1\r\n",
                        "PING" => b"+PONG\r\n",
                        "HELLO" => b"-ERR unknown command\r\n",
                        _ => b"+OK\r\n",
                    };
                    let _ = tx.send(cmd);
                    if write.write_all(reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (url, rx)
}

fn drain(rx: &mut mpsc::UnboundedReceiver<Command>) -> Vec<Command> {
    let mut all = Vec::new();
    while let Ok(c) = rx.try_recv() {
        all.push(c);
    }
    all
}

#[tokio::test]
async fn the_publisher_authenticates_as_its_acl_user_before_publishing() {
    let (url, mut rx) = fake_valkey().await;
    // Characters a URL would mangle: the password must go to AUTH verbatim, never via a URL.
    let password = "p@ss:w/rd%20#?";
    let cache = ProxyCache::with_auth(&url, "control", password).unwrap();
    assert_eq!(cache.password_updated("ep-a", "app").await.unwrap(), 1);
    let commands = drain(&mut rx);
    let auth = commands
        .iter()
        .position(|c| c[0].eq_ignore_ascii_case("AUTH"))
        .unwrap_or_else(|| panic!("no AUTH sent: {commands:?}"));
    assert_eq!(
        commands[auth][1..],
        ["control".to_string(), password.to_string()],
        "{commands:?}"
    );
    let publish = commands
        .iter()
        .position(|c| c[0].eq_ignore_ascii_case("PUBLISH"))
        .unwrap_or_else(|| panic!("no PUBLISH sent: {commands:?}"));
    assert!(auth < publish, "AUTH comes first: {commands:?}");
    assert_eq!(commands[publish][1], CHANNEL);
}

#[tokio::test]
async fn a_publisher_without_credentials_sends_no_auth() {
    let (url, mut rx) = fake_valkey().await;
    let cache = ProxyCache::new(&url).unwrap();
    assert_eq!(cache.password_updated("ep-a", "app").await.unwrap(), 1);
    let commands = drain(&mut rx);
    assert!(
        !commands.iter().any(|c| c[0].eq_ignore_ascii_case("AUTH")),
        "{commands:?}"
    );
}

#[test]
fn an_empty_user_or_password_is_refused() {
    assert!(ProxyCache::with_auth("redis://127.0.0.1:1", "", "pw").is_err());
    assert!(ProxyCache::with_auth("redis://127.0.0.1:1", "control", "").is_err());
}
