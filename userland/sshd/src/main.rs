use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

use russh::keys::PublicKey;
use russh::server::{Auth, Msg, Server, Session};
use russh::{Channel, ChannelId};

struct SshServer;

impl Server for SshServer {
    type Handler = SshSession;

    fn new_client(&mut self, _peer_addr: Option<std::net::SocketAddr>) -> SshSession {
        SshSession {
            channel: None,
            child_stdin: None,
            is_pty: false,
        }
    }
}

struct SshSession {
    channel: Option<Channel<Msg>>,
    child_stdin: Option<std::process::ChildStdin>,
    is_pty: bool,
}

impl SshSession {
    /// Resolve a command name to a full path. Bare names resolve to /bin/<name>.
    fn resolve_program(name: &str) -> String {
        if name.starts_with('/') {
            name.to_string()
        } else {
            format!("/bin/{}", name)
        }
    }

    fn spawn_shell(&mut self, program: &str, args: &[&str]) {
        let channel = self.channel.take().unwrap();
        let (_, write_half) = channel.split();
        let translate_newlines = self.is_pty;

        let path = Self::resolve_program(program);
        let mut child = match Command::new(&path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("sshd: failed to spawn {}: {:?}\r\n", path, e);
                tokio::spawn(async move {
                    write_half.data(msg.as_bytes()).await.ok();
                    write_half.exit_status(127).await.ok();
                    write_half.eof().await.ok();
                    write_half.close().await.ok();
                });
                return;
            }
        };

        self.child_stdin = child.stdin.take();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();

        // Reader threads: blocking reads from child stdout/stderr → shared mpsc channel
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
        let tx2 = tx.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 65536];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 65536];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx2.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // Forwarder task: mpsc → SSH channel
        tokio::spawn(async move {
            while let Some(data) = rx.recv().await {
                if translate_newlines {
                    // Translate \n → \r\n for SSH terminal (no PTY layer to do this)
                    let mut out = Vec::with_capacity(data.len() * 2);
                    for &b in &data {
                        if b == b'\n' {
                            out.push(b'\r');
                        }
                        out.push(b);
                    }
                    if write_half.data(&out[..]).await.is_err() {
                        break;
                    }
                } else {
                    // Binary-safe: send data as-is (SCP, SFTP, etc.)
                    if write_half.data(&data[..]).await.is_err() {
                        break;
                    }
                }
            }
            let status = child.wait().map(|s| s.code().unwrap_or(1) as u32).unwrap_or(1);
            write_half.exit_status(status).await.ok();
            write_half.eof().await.ok();
            write_half.close().await.ok();
        });
    }
}

impl russh::server::Handler for SshSession {
    type Error = russh::Error;

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.channel = Some(channel);
        Ok(true)
    }

    async fn auth_password(
        &mut self,
        _user: &str,
        _password: &str,
    ) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        _key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn data(
        &mut self,
        _channel_id: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(ref mut stdin) = self.child_stdin {
            stdin.write_all(data).ok();
        }
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel_id: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel_id)?;
        self.spawn_shell("/bin/shell", &[]);
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel_id)?;
        let cmd = std::str::from_utf8(data).unwrap_or("").trim();
        // Run through shell so redirects, pipes, etc. work
        self.spawn_shell("/bin/shell", &["-c", cmd]);
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.is_pty = true;
        session.channel_success(channel)?;
        Ok(())
    }
}

fn main() {
    println!("sshd: starting...");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    rt.block_on(async {
        let config = russh::server::Config {
            auth_rejection_time: std::time::Duration::from_secs(1),
            nodelay: true,
            keys: vec![
                russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                    .unwrap(),
            ],
            ..Default::default()
        };
        let config = Arc::new(config);

        let listener = tokio::net::TcpListener::bind("0.0.0.0:22").await.unwrap();
        println!("sshd: listening on port 22");
        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    println!("sshd: connection from {}", addr);
                    let config = config.clone();
                    let handler = SshServer.new_client(Some(addr));
                    tokio::spawn(async move {
                        match russh::server::run_stream(config, stream, handler).await {
                            Ok(session) => {
                                if let Err(e) = session.await {
                                    println!("sshd: session error: {:?}", e);
                                }
                            }
                            Err(e) => {
                                println!("sshd: run_stream error: {:?}", e);
                            }
                        }
                    });
                }
                Err(e) => {
                    println!("sshd: accept error: {:?}", e);
                }
            }
        }
    });
}
