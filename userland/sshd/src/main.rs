use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

use russh::keys::ssh_key::authorized_keys::AuthorizedKeys;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey};
use russh::server::{Auth, Msg, Server, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};

/// Where this machine keeps its SSH identity and the keys it trusts.
///
/// `/home` is the only mount that is both persistent and writable by userland:
/// `/boot` is `KernelOnly` because a process that can write it can make the
/// machine unbootable, `/tmp` is a tmpfs, and `/log` is the diagnostic
/// partition — it is FAT32 by design so that it can be read on another
/// machine, which is the last place a private key should be. On a machine
/// whose disk the kernel would not adopt, `/home` is itself a tmpfs and the
/// identity lasts one boot; the fingerprint is printed every start so that is
/// visible rather than silent.
///
/// There is no user model and no file permissions, so the host key is readable
/// by every process on the machine. That is a property of the system, not of
/// this daemon — see `specs/issues/`.
const SSH_DIR: &str = "/home/root/.ssh";
const HOST_KEY: &str = "/home/root/.ssh/host_ed25519";
const AUTHORIZED_KEYS: &str = "/home/root/.ssh/authorized_keys";

/// The machine's identity, minted once and kept.
///
/// A file that exists but does not parse is refused, never replaced. Minting
/// over it would change the identity every client has pinned without anyone
/// asking, which is the one event a host key exists to make noisy.
fn host_key() -> Result<PrivateKey, String> {
    match fs::read(HOST_KEY) {
        Ok(pem) => PrivateKey::from_openssh(&pem).map_err(|e| {
            format!(
                "{HOST_KEY} is not an OpenSSH private key ({e}); refusing to \
                 replace it — move it aside to mint a new identity"
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => mint_host_key(),
        Err(e) => Err(format!("cannot read {HOST_KEY}: {e}")),
    }
}

fn mint_host_key() -> Result<PrivateKey, String> {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .map_err(|e| format!("cannot generate a host key: {e}"))?;
    let pem = key
        .to_openssh(LineEnding::LF)
        .map_err(|e| format!("cannot encode the host key: {e}"))?;
    fs::create_dir_all(SSH_DIR).map_err(|e| format!("cannot create {SSH_DIR}: {e}"))?;
    fs::write(HOST_KEY, pem.as_bytes()).map_err(|e| format!("cannot write {HOST_KEY}: {e}"))?;
    println!("sshd: minted a new host identity at {HOST_KEY}");
    Ok(key)
}

/// Does `text` — the contents of an `authorized_keys` file — name `offered`?
///
/// Keys are compared as key *data*, so a differing comment is still the same
/// key and an unusual-but-valid encoding still matches; the parse is
/// `ssh-key`'s, which is also what verified the signature.
///
/// **An entry carrying config options authorizes nothing.** Options restrict
/// what a key may do (`command="…"`, `from="…"`, `restrict`), none of them are
/// implemented here, and honouring the key while dropping its restrictions
/// would grant strictly more than the file says.
fn authorizes(text: &str, offered: &PublicKey) -> bool {
    AuthorizedKeys::new(text)
        .filter_map(Result::ok)
        .filter(|entry| entry.config_opts().is_empty())
        .any(|entry| entry.public_key().key_data() == offered.key_data())
}

/// Read fresh on every attempt, so a key added to the file takes effect
/// without a restart — there is nothing here to send a reload signal to.
/// An unreadable file names nobody, so every failure answers "not authorized".
fn is_authorized(key: &PublicKey) -> bool {
    fs::read_to_string(AUTHORIZED_KEYS).is_ok_and(|text| authorizes(&text, key))
}

/// What the file names, said once at startup so a key that will never work is
/// visible before somebody tries it. `Err` means nobody can authenticate.
fn authorized_key_count() -> Result<usize, String> {
    let text = fs::read_to_string(AUTHORIZED_KEYS).map_err(|e| {
        format!("cannot read {AUTHORIZED_KEYS} ({e}); put a public key there and start again")
    })?;

    let (mut usable, mut restricted, mut unreadable) = (0, 0, 0);
    for entry in AuthorizedKeys::new(&text) {
        match entry {
            Ok(entry) if entry.config_opts().is_empty() => usable += 1,
            Ok(_) => restricted += 1,
            Err(_) => unreadable += 1,
        }
    }
    if restricted > 0 {
        println!(
            "sshd: {restricted} entr(ies) in {AUTHORIZED_KEYS} carry options, which are not \
             implemented — those keys authorize nothing"
        );
    }
    if unreadable > 0 {
        println!("sshd: {unreadable} line(s) in {AUTHORIZED_KEYS} are not public keys, ignored");
    }
    if usable == 0 {
        return Err(format!("{AUTHORIZED_KEYS} names no usable key"));
    }
    Ok(usable)
}

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

    /// The offer, before the client has proved it holds the key. Refusing here
    /// is what stops a client signing for a key that could never be accepted,
    /// and it is where an unauthorized key gets named — a client that takes
    /// this answer never reaches `auth_publickey`.
    ///
    /// `russh`'s default for this one is `Accept`; every other auth callback it
    /// defaults to `Reject`, which is why `auth_password` is simply absent.
    async fn auth_publickey_offered(
        &mut self,
        user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        if is_authorized(key) {
            return Ok(Auth::Accept);
        }
        println!(
            "sshd: refused {user}: {} is not in {AUTHORIZED_KEYS}",
            key.fingerprint(HashAlg::Sha256)
        );
        Ok(Auth::reject())
    }

    /// After russh has verified the signature. Checked again rather than
    /// trusting the offer above to have filtered: a client is free to sign
    /// without asking first, and that path must reach the same file.
    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        let fingerprint = key.fingerprint(HashAlg::Sha256);
        if is_authorized(key) {
            println!("sshd: {user} authenticated with {fingerprint}");
            return Ok(Auth::Accept);
        }
        println!("sshd: refused {user}: {fingerprint} is not in {AUTHORIZED_KEYS}");
        Ok(Auth::reject())
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
        // Every bind goes through netd, which exits on a machine with no NIC.
        // sshd has nothing to offer without one, so it says so and leaves
        // instead of dumping a tokio backtrace across the boot.
        //
        // Only for that error, though. `NetdNotFound` — no netd registered the
        // service name — is the one that means what the message says, and std
        // maps it to NotConnected. `AddrInUse`, a netd that died mid-request
        // (`NetError::Io`) and a pipe failure all arrive here too, and on a
        // laptop with a live link every one of them would have exited 0 with a
        // line blaming the hardware. Nothing supervises init's children, so
        // the message is the entire diagnostic.
        let listener = match tokio::net::TcpListener::bind("0.0.0.0:22").await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::NotConnected => {
                println!("sshd: no network on this machine, exiting");
                return;
            }
            Err(e) => panic!("sshd: cannot bind 0.0.0.0:22: {e}"),
        };

        // Identity and trust are settled after the bind, so that a machine with
        // no NIC still reports the network as the reason it is leaving rather
        // than minting a key it will never present.
        let host_key = match host_key() {
            Ok(key) => key,
            Err(why) => {
                println!("sshd: {why}, exiting");
                return;
            }
        };
        println!(
            "sshd: host identity {}",
            host_key.public_key().fingerprint(HashAlg::Sha256)
        );

        // A daemon that can authenticate nobody is an open port, not a service.
        match authorized_key_count() {
            Ok(count) => println!("sshd: {count} key(s) authorized by {AUTHORIZED_KEYS}"),
            Err(why) => {
                println!("sshd: {why}, exiting");
                return;
            }
        }

        let config = Arc::new(russh::server::Config {
            // Public keys and nothing else: password and keyboard-interactive
            // are never offered, so there is no credential for a client to
            // guess. `russh`'s default is every method it implements.
            methods: MethodSet::from(&[MethodKind::PublicKey][..]),
            auth_rejection_time: std::time::Duration::from_secs(1),
            nodelay: true,
            keys: vec![host_key],
            ..Default::default()
        });

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

/// Which keys a file authorizes. Host tests — `cargo test --target "$(rustc
/// -vV | sed -n 's/^host: //p')"` from this directory; `userland/.cargo/config.toml`
/// cross-compiles to ToyOS otherwise. Real keys, and `ssh-key`'s own parser,
/// so what is under test is the decision and not a re-encoding of it.
#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> PrivateKey {
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap()
    }

    /// One `authorized_keys` line for a key, as `ssh-keygen` would write it.
    fn line(key: &PrivateKey) -> String {
        key.public_key().to_openssh().unwrap()
    }

    #[test]
    fn a_listed_key_is_authorized() {
        let mine = key();
        assert!(authorizes(&line(&mine), mine.public_key()));
    }

    #[test]
    fn an_unlisted_key_is_not_authorized() {
        let (mine, stranger) = (key(), key());
        assert!(!authorizes(&line(&mine), stranger.public_key()));
    }

    #[test]
    fn a_file_that_names_nobody_authorizes_nobody() {
        let stranger = key();
        for text in ["", "\n", "   \n\t\n", "# just a comment\n", "garbage\n"] {
            assert!(
                !authorizes(text, stranger.public_key()),
                "{text:?} authorized a key",
            );
        }
    }

    /// The load-bearing refusal: the key is listed, but under restrictions this
    /// daemon does not implement. Granting it would grant more than the file
    /// says, so it is granted nothing.
    #[test]
    fn a_key_listed_with_options_authorizes_nothing() {
        let mine = key();
        for options in [
            "command=\"/bin/shell -c ls\"",
            "no-pty",
            "restrict",
            "from=\"10.0.0.1\",no-agent-forwarding",
        ] {
            let text = format!("{options} {}\n", line(&mine));
            assert!(
                !authorizes(&text, mine.public_key()),
                "{options:?} let the key through unrestricted",
            );
        }
    }

    /// Keys are compared as key data, so the comment is not part of identity.
    #[test]
    fn the_comment_is_not_part_of_the_key() {
        let mine = key();
        let text = format!("{} jan@some-other-laptop\n", line(&mine));
        assert!(authorizes(&text, mine.public_key()));
    }

    #[test]
    fn a_key_is_found_among_several_and_blank_lines() {
        let (first, mine, last) = (key(), key(), key());
        let text = format!(
            "# my keys\n{}\n\n{}\n   \n{}\n",
            line(&first),
            line(&mine),
            line(&last),
        );
        for k in [&first, &mine, &last] {
            assert!(authorizes(&text, k.public_key()), "a listed key was missed");
        }
        assert!(!authorizes(&text, key().public_key()));
    }

    /// A line that is not a key must not disarm the keys around it.
    #[test]
    fn an_unparseable_line_does_not_disarm_the_rest() {
        let mine = key();
        let text = format!("not-a-key at all\n{}\n", line(&mine));
        assert!(authorizes(&text, mine.public_key()));
    }
}
