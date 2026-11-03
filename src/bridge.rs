//! A loopback TCP port inside a sandbox that pipes to the broker's Unix
//! socket, for a harness whose model endpoint must be an HTTP URL, such as
//! Claude Code's `ANTHROPIC_BASE_URL`. The sandbox has its own network
//! namespace, so nothing outside it reaches the port, and the bridge adds
//! nothing to what crosses: the broker authenticates every request.
//! `solos-swarm bridge` binds the port, runs the harness and exits with it.

use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitStatus;
use tokio::net::{TcpListener, UnixStream};
use tokio::process::Command;

/// Where the bridge listens inside a sandbox.
pub const SANDBOX_ADDR: &str = "127.0.0.1:41300";

pub struct Bridge {
    listener: TcpListener,
    socket: PathBuf,
}

impl Bridge {
    /// Listen on `addr` for connections to pipe to `socket`.
    pub async fn bind(addr: SocketAddr, socket: PathBuf) -> Result<Self> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("cannot listen on {addr}"))?;
        Ok(Self { listener, socket })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// Run `program` with this process's standard streams, piping each
    /// connection to the socket until it exits. The port is bound first, so
    /// the program never finds it closed.
    pub async fn run(self, program: &OsStr, args: &[impl AsRef<OsStr>]) -> Result<ExitStatus> {
        let mut child = Command::new(program)
            .args(args)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("cannot start {}", program.to_string_lossy()))?;
        loop {
            tokio::select! {
                status = child.wait() => return status.context("cannot wait for the program"),
                accepted = self.listener.accept() => {
                    let Ok((mut tcp, _)) = accepted else { continue };
                    let socket = self.socket.clone();
                    tokio::spawn(async move {
                        if let Ok(mut unix) = UnixStream::connect(&socket).await {
                            let _ = tokio::io::copy_bidirectional(&mut tcp, &mut unix).await;
                        }
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Bridge;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpStream, UnixListener};

    #[tokio::test]
    async fn each_connection_reaches_the_socket_while_the_program_runs() {
        let dir = std::env::temp_dir().join(format!("bridge-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("broker.sock");
        let server = UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = server.accept().await {
                tokio::spawn(async move {
                    let mut asked = [0; 4];
                    stream.read_exact(&mut asked).await.unwrap();
                    asked.reverse();
                    stream.write_all(&asked).await.unwrap();
                });
            }
        });
        let bridge = Bridge::bind("127.0.0.1:0".parse().unwrap(), socket)
            .await
            .unwrap();
        let addr = bridge.local_addr().unwrap();
        let running = tokio::spawn(bridge.run("/bin/sleep".as_ref(), &["1"]));
        for word in [*b"ping", *b"abcd"] {
            let mut tcp = TcpStream::connect(addr).await.unwrap();
            tcp.write_all(&word).await.unwrap();
            let mut answer = [0; 4];
            tcp.read_exact(&mut answer).await.unwrap();
            let mut expected = word;
            expected.reverse();
            assert_eq!(answer, expected);
        }
        let status = running.await.unwrap().unwrap();
        assert!(status.success());
        assert!(
            TcpStream::connect(addr).await.is_err(),
            "the port closes with the program"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
