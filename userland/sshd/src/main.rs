use std::sync::Arc;

use russh::keys::ssh_key::rand_core::OsRng;
use russh::keys::PublicKey;
use russh::server::{Auth, Msg, Server, Session};
use russh::{Channel, ChannelId, CryptoVec};

struct SshServer;

impl Server for SshServer {
    type Handler = SshSession;

    fn new_client(&mut self, _peer_addr: Option<std::net::SocketAddr>) -> SshSession {
        SshSession
    }
}

struct SshSession;

impl russh::server::Handler for SshSession {
    type Error = russh::Error;

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
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
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Echo data back
        session.data(channel_id, CryptoVec::from_slice(data))?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel_id: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel_id)?;
        session.data(channel_id, CryptoVec::from_slice(b"Welcome to ToyOS!\r\n"))?;
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
        session.channel_success(channel)?;
        Ok(())
    }
}

fn main() {
    println!("sshd: building tokio runtime...");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    println!("sshd: runtime built, calling block_on...");
    rt.block_on(async {
        println!("sshd: inside async block, configuring...");
        let config = russh::server::Config {
            auth_rejection_time: std::time::Duration::from_secs(1),
            keys: vec![
                russh::keys::PrivateKey::random(&mut OsRng, russh::keys::Algorithm::Ed25519)
                    .unwrap(),
            ],
            ..Default::default()
        };
        let config = Arc::new(config);

        println!("sshd: starting server on 0.0.0.0:22...");
        let listener = tokio::net::TcpListener::bind("0.0.0.0:22").await.unwrap();
        println!("sshd: bound, waiting for connections...");
        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    println!("sshd: accepted connection from {}", addr);
                    let config = config.clone();
                    let handler = SshServer.new_client(Some(addr));
                    tokio::spawn(async move {
                        match russh::server::run_stream(config, stream, handler).await {
                            Ok(session) => {
                                println!("sshd: session started for {}", addr);
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
