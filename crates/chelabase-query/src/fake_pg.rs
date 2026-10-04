//! A fake Postgres server for unit tests: it accepts every connection, answers the startup
//! with `AuthenticationOk` and `ReadyForQuery`, then answers the extended protocol's
//! prepares (`Parse`, `Describe`, `Close`, `Sync`: each statement takes the parameters it
//! declared and returns no rows) and every simple query with `CommandComplete`. Nothing it
//! answers carries data.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A message: its type byte and a body.
fn message(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![kind];
    out.extend_from_slice(&i32::try_from(body.len() + 4).unwrap().to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// The parameter types a `Parse` body declares (after the statement name and the query).
fn declared_types(body: &[u8]) -> Vec<u8> {
    let name_end = body.iter().position(|b| *b == 0).unwrap();
    let query_end = name_end + 1 + body[name_end + 1..].iter().position(|b| *b == 0).unwrap();
    let rest = &body[query_end + 1..];
    let n = usize::try_from(i16::from_be_bytes([rest[0], rest[1]])).unwrap();
    let mut out = rest[..2].to_vec();
    out.extend_from_slice(&rest[2..2 + 4 * n]);
    out
}

async fn serve(mut socket: TcpStream) {
    let Ok(len) = socket.read_i32().await else {
        return;
    };
    let mut startup = vec![0; usize::try_from(len).unwrap() - 4];
    if socket.read_exact(&mut startup).await.is_err() {
        return;
    }
    let ready = message(b'Z', b"I");
    let mut hello = message(b'R', &0_i32.to_be_bytes());
    hello.extend_from_slice(&ready);
    if socket.write_all(&hello).await.is_err() {
        return;
    }
    let mut pending = Vec::new();
    // The last `Parse`'s declared types, for its `Describe`.
    let mut types = vec![0, 0];
    loop {
        let Ok(kind) = socket.read_u8().await else {
            return;
        };
        let Ok(len) = socket.read_i32().await else {
            return;
        };
        let mut body = vec![0; usize::try_from(len).unwrap() - 4];
        if socket.read_exact(&mut body).await.is_err() {
            return;
        }
        match kind {
            b'P' => {
                types = declared_types(&body);
                pending.extend(message(b'1', &[]));
            }
            b'D' => {
                pending.extend(message(b't', &types));
                pending.extend(message(b'n', &[]));
            }
            b'C' => pending.extend(message(b'3', &[])),
            b'S' => {
                pending.extend(&ready);
                if socket.write_all(&pending).await.is_err() {
                    return;
                }
                pending.clear();
            }
            b'Q' => {
                let mut out = message(b'C', b"OK\0");
                out.extend_from_slice(&ready);
                if socket.write_all(&out).await.is_err() {
                    return;
                }
            }
            b'X' => return,
            _ => {}
        }
    }
}

/// A pool of up to `max_size` connections to a new fake server.
pub(crate) async fn pool(max_size: usize) -> deadpool_postgres::Pool {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(serve(socket));
        }
    });
    let mut config = tokio_postgres::Config::new();
    config.host("127.0.0.1").port(port).user("x").dbname("x");
    let manager = deadpool_postgres::Manager::from_config(
        config,
        tokio_postgres::NoTls,
        deadpool_postgres::ManagerConfig {
            recycling_method: deadpool_postgres::RecyclingMethod::Fast,
        },
    );
    deadpool_postgres::Pool::builder(manager)
        .max_size(max_size)
        .runtime(deadpool_postgres::Runtime::Tokio1)
        .build()
        .unwrap()
}
