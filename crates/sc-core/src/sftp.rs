//! SFTP: hosts as `~/.ssh/config` names them, keys from the agent or from
//! files, and the server's key checked against `known_hosts` as `ssh` does.

use std::any::Any;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use russh::client::{self, Handle, Handler};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{self, HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{MethodKind, MethodSet};
use russh_sftp::client::SftpSession;
use russh_sftp::client::fs::{File, Metadata};
use russh_sftp::protocol::{FileAttributes, StatusCode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tokio::runtime::Runtime;

use crate::Entry;
use crate::remote::{Address, Ask, Prompt, Reply};
use crate::secrets;
use crate::vfs::{Fs, Meta, Remote};

/// Every connection's network work; callers block on it from their own threads.
static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("ssh")
        .enable_all()
        .build()
        .expect("a runtime for SSH")
});

/// The session and what carries it: SSH, or the program it is piped to.
type Connection = (Arc<SftpSession>, Box<dyn Any + Send + Sync>);

pub struct Sftp {
    url: String,
    home: PathBuf,
    /// Where and how it logged in, to log in again once SSH's connection is lost.
    again: Option<(Address, Ask)>,
    /// Taken in `drop`, where the runtime must be at hand.
    inner: Mutex<Option<Connection>>,
}

impl Sftp {
    /// Connects and logs in, asking `ask` what it must. Blocks: run it off
    /// the UI thread.
    pub fn connect(address: &Address, ask: Ask) -> io::Result<Arc<Self>> {
        let mut sftp = RUNTIME.block_on(connect(address, ask.clone()))?;
        sftp.again = Some((address.clone(), ask));
        Ok(Arc::new(sftp))
    }

    /// Where `address` points, else the login folder.
    pub fn start(&self, address: &Address) -> PathBuf {
        address
            .path
            .clone()
            .map_or_else(|| self.home.clone(), PathBuf::from)
    }

    /// SFTP on a program's standard input and output, as `rclone serve sftp
    /// --stdio` speaks it, ended with the connection. Blocks.
    pub(crate) fn over_pipe(url: String, command: std::process::Command) -> io::Result<Arc<Self>> {
        use std::process::Stdio;
        let mut command = tokio::process::Command::from(command);
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        RUNTIME.block_on(async {
            let mut child = command.spawn()?;
            let (Some(output), Some(input), Some(mut errors)) = (child.stdout.take(), child.stdin.take(), child.stderr.take()) else {
                return Err(io::Error::other("no pipe to the program"));
            };
            // Its last words, for a failed start; read always, so it never waits on a full pipe.
            let said = tokio::spawn(async move {
                let (mut tail, mut buffer) = (Vec::new(), [0; 4096]);
                while let Ok(read @ 1..) = errors.read(&mut buffer).await {
                    tail.extend_from_slice(&buffer[..read]);
                    tail.drain(..tail.len().saturating_sub(buffer.len()));
                }
                String::from_utf8_lossy(&tail).into_owned()
            });
            let started = async {
                let session = SftpSession::new(tokio::io::join(output, input)).await.map_err(other)?;
                let home = session.canonicalize(".").await.map_err(sftp_error)?;
                Ok::<_, io::Error>((session, home))
            };
            match started.await {
                Ok((session, home)) => Ok(Sftp { url, home: home.into(), again: None, inner: Mutex::new(Some((Arc::new(session), Box::new(child)))) }),
                Err(err) => {
                    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                    let _ = child.start_kill();
                    let said = tokio::time::timeout(Duration::from_secs(1), said).await.ok().and_then(Result::ok);
                    Err(said.and_then(|said| last_error(&said)).map_or(err, io::Error::other))
                }
            }
        })
        .map(Arc::new)
    }

    /// SSH's connection lost (a broken network, a server restarted): logged
    /// in again, so Retry goes on.
    fn session(&self) -> Arc<SftpSession> {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let lost = inner.as_ref().and_then(|(_, carrier)| carrier.downcast_ref::<Handle<Client>>()).is_some_and(Handle::is_closed);
        if lost
            && let Some((address, ask)) = &self.again
            // shortcut: not logged in again, the lost session gives the caller its error.
            && let Ok(mut fresh) = RUNTIME.block_on(connect(address, ask.clone()))
        {
            let gone = std::mem::replace(&mut *inner, fresh.inner.get_mut().unwrap_or_else(|err| err.into_inner()).take());
            let _runtime = RUNTIME.enter();
            drop(gone);
        }
        inner.as_ref().expect("connected until dropped").0.clone()
    }
}

impl Drop for Sftp {
    fn drop(&mut self) {
        let _runtime = RUNTIME.enter();
        drop(self.inner.get_mut().unwrap_or_else(|err| err.into_inner()).take());
    }
}

/// What `~/.ssh/config` makes of the address: user, server and port.
fn resolve(address: &Address) -> (russh_config::Config, String, String, u16) {
    let config = russh_config::parse_home(&address.host)
        .unwrap_or_else(|_| russh_config::Config::default(&address.host));
    let host = config.host().to_owned();
    let port = address.port.unwrap_or_else(|| config.port());
    let user = address.user.clone().unwrap_or_else(|| config.user());
    (config, user, host, port)
}

pub(crate) fn keyring_key(address: &Address) -> String {
    let (_, user, host, port) = resolve(address);
    secrets::key("sftp", &user, &host, port)
}

async fn connect(address: &Address, ask: Ask) -> io::Result<Sftp> {
    let (config, user, host, port) = resolve(address);
    let known_hosts = config
        .host_config
        .user_known_hosts_file
        .clone()
        .unwrap_or_else(|| {
            let home = std::env::home_dir().unwrap_or_default();
            home.join(".ssh").join("known_hosts")
        });
    let refused = Arc::new(Mutex::new(None));
    let client = Client {
        host: host.clone(),
        port,
        known_hosts,
        ask: ask.clone(),
        refused: refused.clone(),
    };
    let settings = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });
    let connected = client::connect(settings, (host.as_str(), port), client).await;
    let mut ssh = connected.map_err(|err| match refused.lock().unwrap().take() {
        Some(why) => io::Error::new(io::ErrorKind::PermissionDenied, why),
        None => io::Error::other(format!("{host}:{port}: {err}")),
    })?;
    let key = secrets::key("sftp", &user, &host, port);
    login(&mut ssh, &user, &host, &key, &config, &ask).await?;
    let channel = ssh.channel_open_session().await.map_err(other)?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(other)?;
    let session = SftpSession::new(channel.into_stream())
        .await
        .map_err(other)?;
    let home = PathBuf::from(session.canonicalize(".").await.map_err(sftp_error)?);
    let url = match address.port {
        Some(port) => format!("sftp://{user}@{}:{port}", address.host),
        None => format!("sftp://{user}@{}", address.host),
    };
    Ok(Sftp {
        url,
        home,
        again: None,
        inner: Mutex::new(Some((Arc::new(session), Box::new(ssh)))),
    })
}

struct Client {
    host: String,
    port: u16,
    known_hosts: PathBuf,
    ask: Ask,
    /// Why the server's key was refused, for the error.
    refused: Arc<Mutex<Option<String>>>,
}

impl Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let refuse = |why: String| {
            *self.refused.lock().unwrap() = Some(why);
            Ok(false)
        };
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            return refuse(
                "The server shows a host certificate; those are not supported yet.".into(),
            );
        };
        let server = match self.port {
            22 => self.host.clone(),
            port => format!("{}:{port}", self.host),
        };
        match keys::check_known_hosts_path(&self.host, self.port, key, &self.known_hosts) {
            Ok(true) => Ok(true),
            Err(keys::Error::KeyChanged { line }) => refuse(format!(
                "The host key of {server} has CHANGED since line {line} of {} recorded it. \
                 Someone could be listening in; if the server was set up anew, remove that line.",
                self.known_hosts.display()
            )),
            // Unknown, or no known_hosts file yet.
            _ => {
                let prompt = Prompt::UnknownHost {
                    host: server.clone(),
                    key_type: key.algorithm().to_string(),
                    fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
                };
                match ask_user(&self.ask, prompt).await {
                    Reply::Trust { remember } => {
                        if remember {
                            // shortcut: an unwritable known_hosts only means asking again next time.
                            let _ = keys::known_hosts::learn_known_hosts_path(
                                &self.host,
                                self.port,
                                key,
                                &self.known_hosts,
                            );
                        }
                        Ok(true)
                    }
                    _ => refuse(format!("The host key of {server} was not trusted.")),
                }
            }
        }
    }
}

async fn ask_user(ask: &Ask, prompt: Prompt) -> Reply {
    let ask = ask.clone();
    tokio::task::spawn_blocking(move || ask(prompt))
        .await
        .unwrap_or(Reply::Cancel)
}

/// As `ssh`: the agent's keys, the key files, then a password, three times.
async fn login(
    ssh: &mut Handle<Client>,
    user: &str,
    host: &str,
    key: &str,
    config: &russh_config::Config,
    ask: &Ask,
) -> io::Result<()> {
    let mut methods = None;
    if let Some(agent) = agent().await
        && with_agent(ssh, user, agent, &mut methods).await
    {
        return Ok(());
    }
    let home = std::env::home_dir().unwrap_or_default();
    let defaults = ["id_ed25519", "id_ecdsa", "id_rsa"].map(|name| home.join(".ssh").join(name));
    let files = config
        .host_config
        .identity_file
        .clone()
        .unwrap_or_else(|| defaults.to_vec());
    for file in files.iter().filter(|file| file.exists()) {
        let key = match keys::load_secret_key(file, None) {
            Ok(key) => key,
            Err(keys::Error::KeyIsEncrypted) => {
                let prompt = Prompt::Secret {
                    title: "Key passphrase".into(),
                    text: format!("Passphrase for {}", file.display()),
                    keyring: None,
                };
                let Reply::Secret(passphrase) = ask_user(ask, prompt).await else {
                    continue;
                };
                match keys::load_secret_key(file, Some(&passphrase)) {
                    Ok(key) => key,
                    Err(_) => continue,
                }
            }
            Err(_) => continue,
        };
        let hash = match key.algorithm().is_rsa() {
            true => ssh.best_supported_rsa_hash().await.ok().flatten().flatten(),
            false => None,
        };
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
        match ssh.authenticate_publickey(user, key).await.map_err(other)? {
            client::AuthResult::Success => return Ok(()),
            client::AuthResult::Failure {
                remaining_methods, ..
            } => methods = Some(remaining_methods),
        }
    }
    let offered = |kind| {
        methods
            .as_ref()
            .is_none_or(|methods: &MethodSet| methods.contains(&kind))
    };
    let (password, interactive) = (
        offered(MethodKind::Password),
        offered(MethodKind::KeyboardInteractive),
    );
    if !password && !interactive {
        let refused = format!("{user}@{host}: the server refused the keys and takes no password");
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, refused));
    }
    for _ in 0..3 {
        let prompt = Prompt::Secret {
            title: "Password".into(),
            text: format!("Password for {user}@{host}"),
            keyring: Some(key.to_owned()),
        };
        let Reply::Secret(secret) = ask_user(ask, prompt).await else {
            break;
        };
        if password
            && ssh
                .authenticate_password(user, &secret)
                .await
                .map_err(other)?
                .success()
        {
            return Ok(());
        }
        if interactive && keyboard_interactive(ssh, user, &secret).await? {
            return Ok(());
        }
    }
    let refused = format!("{user}@{host}: login refused");
    Err(io::Error::new(io::ErrorKind::PermissionDenied, refused))
}

/// Answers the server's prompts with the password: one-time codes are not asked for yet.
async fn keyboard_interactive(
    ssh: &mut Handle<Client>,
    user: &str,
    secret: &str,
) -> io::Result<bool> {
    use client::KeyboardInteractiveAuthResponse as Response;
    let mut response = ssh
        .authenticate_keyboard_interactive_start(user, None)
        .await
        .map_err(other)?;
    loop {
        match response {
            Response::Success => return Ok(true),
            Response::Failure { .. } => return Ok(false),
            Response::InfoRequest { prompts, .. } => {
                let answers = prompts.iter().map(|_| secret.to_owned()).collect();
                response = ssh
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(other)?;
            }
        }
    }
}

#[cfg(unix)]
async fn agent() -> Option<AgentClient<tokio::net::UnixStream>> {
    AgentClient::connect_env().await.ok()
}

#[cfg(windows)]
async fn agent() -> Option<AgentClient<tokio::net::windows::named_pipe::NamedPipeClient>> {
    AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
        .await
        .ok()
}

async fn with_agent<S>(
    ssh: &mut Handle<Client>,
    user: &str,
    mut agent: AgentClient<S>,
    methods: &mut Option<MethodSet>,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let identities = agent.request_identities().await.unwrap_or_default();
    for identity in identities {
        let AgentIdentity::PublicKey { key, .. } = identity else {
            continue;
        };
        let hash = match key.algorithm().is_rsa() {
            true => ssh.best_supported_rsa_hash().await.ok().flatten().flatten(),
            false => None,
        };
        match ssh
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
        {
            Ok(client::AuthResult::Success) => return true,
            Ok(client::AuthResult::Failure {
                remaining_methods, ..
            }) => *methods = Some(remaining_methods),
            Err(_) => {}
        }
    }
    false
}

/// A program's last error line, without rclone's time and level.
fn last_error(said: &str) -> Option<String> {
    let line = said.lines().rev().find(|line| line.contains("ERROR") || line.contains("CRITICAL") || line.contains("Fatal"))?;
    let message = ["Fatal error: ", "CRITICAL: ", "ERROR : "].iter().find_map(|level| line.split_once(level).map(|(_, rest)| rest));
    Some(message.unwrap_or(line).trim().to_owned())
}

fn other(err: impl std::fmt::Display) -> io::Error {
    io::Error::other(err.to_string())
}

fn sftp_error(err: russh_sftp::client::error::Error) -> io::Error {
    use russh_sftp::client::error::Error;
    match &err {
        Error::Status(status) if status.status_code == StatusCode::NoSuchFile => {
            io::Error::new(io::ErrorKind::NotFound, status.error_message.clone())
        }
        Error::Status(status) if status.status_code == StatusCode::PermissionDenied => {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                status.error_message.clone(),
            )
        }
        _ => other(err),
    }
}

/// SFTP paths are POSIX, whatever the local system's are.
fn text(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn time(seconds: Option<u32>) -> Option<SystemTime> {
    seconds.map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds.into()))
}

fn meta(attrs: &Metadata) -> Meta {
    Meta {
        is_dir: attrs.is_dir(),
        is_link: attrs.is_symlink(),
        size: attrs.size.unwrap_or(0),
        modified: time(attrs.mtime),
        mode: attrs.permissions.map(|mode| mode & 0o7777),
    }
}

/// `rwxr-xr-x`, as local panels show it.
pub(crate) fn mode_text(mode: Option<u32>) -> String {
    let Some(mode) = mode else {
        return String::new();
    };
    (0..9)
        .map(|bit| match mode & (0o400 >> bit) != 0 {
            true => ['r', 'w', 'x'][bit % 3],
            false => '-',
        })
        .collect()
}

fn block_on<T>(
    work: impl Future<Output = Result<T, russh_sftp::client::error::Error>>,
) -> io::Result<T> {
    RUNTIME.block_on(work).map_err(sftp_error)
}

impl Fs for Sftp {
    fn id(&self) -> String {
        self.url.clone()
    }

    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        block_on(self.session().symlink_metadata(text(path))).map(|attrs| meta(&attrs))
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<OsString>> {
        let names = block_on(self.session().read_dir(text(dir)))?.map(|entry| entry.file_name());
        Ok(names
            .filter(|name| name != "." && name != "..")
            .map(OsString::from)
            .collect())
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let file = block_on(self.session().open(text(path)))?;
        Ok(Box::new(SyncFile(Some(file))))
    }

    fn create(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let file = block_on(self.session().create(text(path)))?;
        Ok(Box::new(SyncFile(Some(file))))
    }

    fn open_at(&self, path: &Path, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        let mut file = SyncFile(Some(block_on(self.session().open(text(path)))?));
        let opened = file.0.as_mut().expect("just opened");
        RUNTIME.block_on(opened.seek(io::SeekFrom::Start(offset)))?;
        Ok(Box::new(file))
    }

    /// Written from its size on: servers do not all keep to the append flag.
    fn append(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let size = self.metadata(path)?.size;
        let flags = russh_sftp::protocol::OpenFlags::WRITE;
        let mut file = SyncFile(Some(block_on(self.session().open_with_flags(text(path), flags))?));
        let opened = file.0.as_mut().expect("just opened");
        RUNTIME.block_on(opened.seek(io::SeekFrom::Start(size)))?;
        Ok(Box::new(file))
    }

    fn set_meta(&self, path: &Path, meta: &Meta) -> io::Result<()> {
        let seconds = meta
            .modified
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok());
        let mut attrs = FileAttributes::empty();
        attrs.mtime = seconds.map(|seconds| seconds.as_secs() as u32);
        attrs.atime = attrs.mtime;
        attrs.permissions = meta.mode;
        block_on(self.session().set_metadata(text(path), attrs))
    }

    fn mkdir(&self, path: &Path) -> io::Result<()> {
        block_on(self.session().create_dir(text(path)))
    }

    fn remove(&self, path: &Path, is_dir: bool) -> io::Result<()> {
        match is_dir {
            true => block_on(self.session().remove_dir(text(path))),
            false => block_on(self.session().remove_file(text(path))),
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        block_on(self.session().rename(text(from), text(to)))
    }
}

impl Remote for Sftp {
    fn url(&self) -> String {
        self.url.clone()
    }

    fn home(&self) -> PathBuf {
        self.home.clone()
    }

    /// Links to folders show as folders, as local ones do.
    fn entries(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        let session = self.session();
        RUNTIME
            .block_on(async {
                let mut entries = Vec::new();
                for item in session.read_dir(text(dir)).await? {
                    let name = item.file_name();
                    if name == "." || name == ".." {
                        continue;
                    }
                    let mut attrs = item.metadata();
                    if attrs.is_symlink()
                        && let Ok(target) = session.metadata(text(&dir.join(&name))).await
                    {
                        attrs = target;
                    }
                    entries.push(Entry {
                        hidden: name.starts_with('.'),
                        system: false,
                        attributes: mode_text(attrs.permissions),
                        tags: Vec::new(),
                        online: false,
                        is_dir: attrs.is_dir(),
                        size: attrs.size.unwrap_or(0),
                        modified: time(attrs.mtime),
                        name: name.into(),
                    });
                }
                Ok(entries)
            })
            .map_err(sftp_error)
            .map(|mut entries| {
                if dir.parent().is_some() {
                    entries.insert(0, crate::parent_entry(None));
                }
                entries
            })
    }
}

/// A server's file, read and written from a thread outside the runtime.
struct SyncFile(Option<File>);

impl Read for SyncFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let file = self.0.as_mut().expect("open until dropped");
        RUNTIME.block_on(file.read(buffer))
    }
}

impl Write for SyncFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let file = self.0.as_mut().expect("open until dropped");
        RUNTIME.block_on(file.write(buffer))
    }

    fn flush(&mut self) -> io::Result<()> {
        let file = self.0.as_mut().expect("open until dropped");
        RUNTIME.block_on(file.flush())
    }
}

impl Drop for SyncFile {
    fn drop(&mut self) {
        if let Some(mut file) = self.0.take() {
            // Closes the handle once what was written has arrived.
            let _ = RUNTIME.block_on(file.shutdown());
            let _runtime = RUNTIME.enter();
            drop(file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs PC_SFTP_TEST, and PC_SFTP_CUT: a command that ends the server's
    /// side of the connection. The download broken so goes on from its part.
    #[test]
    #[ignore]
    fn goes_on_after_the_connection_is_lost() {
        use crate::vfs::{self, Local};
        let address = Address::parse(&std::env::var("PC_SFTP_TEST").unwrap()).unwrap();
        let cut = std::env::var("PC_SFTP_CUT").unwrap();
        let ask: Ask = Arc::new(|_| Reply::Cancel);
        let sftp = Sftp::connect(&address, ask).unwrap();
        let local = std::env::temp_dir().join(format!("sc-sftp-lost-{}", std::process::id()));
        std::fs::create_dir_all(&local).unwrap();
        let data: Vec<u8> = (0..8 << 20).map(|at| (at % 251) as u8).collect();
        std::fs::write(local.join("big.bin"), &data).unwrap();
        let remote = sftp.start(&address).join("lost.bin");
        let meta = Local.metadata(&local.join("big.bin")).unwrap();
        vfs::copy_file(&Local, &local.join("big.bin"), &*sftp, &remote, &meta, &mut |_, _| Ok(())).unwrap();
        let (down, meta) = (local.join("down.bin"), sftp.metadata(&remote).unwrap());
        let mut cut_once = Some(cut);
        let broken = vfs::copy_file(&*sftp, &remote, &Local, &down, &meta, &mut |_, _| {
            if let Some(cut) = cut_once.take() {
                std::process::Command::new("sh").arg("-c").arg(cut).status().unwrap();
                std::thread::sleep(Duration::from_millis(300));
            }
            Ok(())
        });
        assert!(broken.is_err());
        let mut resumed = false;
        vfs::copy_file(&*sftp, &remote, &Local, &down, &meta, &mut |_, from_part| Ok(resumed |= from_part)).unwrap();
        assert!(resumed);
        assert_eq!(std::fs::read(&down).unwrap(), data);
        sftp.remove(&remote, false).unwrap();
        std::fs::remove_dir_all(&local).unwrap();
    }

    /// Needs a server: PC_SFTP_TEST=sftp://user@host:port/dir, with a key
    /// it takes in $HOME/.ssh. The first run trusts the host key.
    #[test]
    #[ignore]
    fn talks_to_a_server() {
        use crate::ops::{Kind, Operation, Overwrite, Queue, Status, Target};
        use crate::vfs::Local;
        let url = std::env::var("PC_SFTP_TEST").expect("PC_SFTP_TEST");
        let address = Address::parse(&url).unwrap();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let heard = asked.clone();
        let ask: Ask = Arc::new(move |prompt| {
            heard.lock().unwrap().push(format!("{prompt:?}"));
            match prompt {
                Prompt::UnknownHost { .. } => Reply::Trust { remember: true },
                // PC_SFTP_SECRET answers a key's passphrase.
                Prompt::Secret { .. } => match std::env::var("PC_SFTP_SECRET") {
                    Ok(secret) => Reply::Secret(secret),
                    Err(_) => Reply::Cancel,
                },
            }
        });
        let sftp = Sftp::connect(&address, ask.clone()).unwrap();
        let dir = sftp.start(&address);
        crate::vfs::resumes_both_ways(&*sftp, &dir);
        let names: Vec<String> = sftp
            .entries(&dir)
            .unwrap()
            .iter()
            .map(|e| e.display_name())
            .collect();
        assert!(names.contains(&"hello.txt".to_string()), "{names:?}");
        let mut hello = String::new();
        sftp.open(&dir.join("hello.txt"))
            .unwrap()
            .read_to_string(&mut hello)
            .unwrap();
        assert_eq!(hello, "hello\n");

        // Through the queue: up, and down again.
        let local = std::env::temp_dir().join(format!("pc-sftp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&local);
        std::fs::create_dir_all(local.join("up/sub")).unwrap();
        std::fs::write(local.join("up/sub/big.bin"), vec![7u8; 3 << 20]).unwrap();
        let remote: Arc<dyn Fs> = sftp.clone();
        let run = |op: Operation| {
            let queue = Queue::default();
            let id = queue.push(op);
            loop {
                let job = queue.jobs().into_iter().find(|job| job.id == id).unwrap();
                if job.status.finished() {
                    return job;
                }
                assert!(job.question.is_none(), "{:?}", job.question);
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let up = run(Operation {
            kind: Kind::Copy,
            from: Arc::new(Local),
            sources: vec![local.join("up")],
            to: remote.clone(),
            target: Target::Into(dir.clone()),
            overwrite: Overwrite::All,
        });
        assert_eq!((up.status, up.progress.bytes), (Status::Done, 3 << 20));
        assert_eq!(
            sftp.metadata(&dir.join("up/sub/big.bin")).unwrap().size,
            3 << 20
        );
        let down = run(Operation {
            kind: Kind::Move,
            from: remote.clone(),
            sources: vec![dir.join("up")],
            to: Arc::new(Local),
            target: Target::As(local.join("down")),
            overwrite: Overwrite::All,
        });
        assert_eq!(down.status, Status::Done);
        assert_eq!(
            std::fs::read(local.join("down/sub/big.bin")).unwrap(),
            vec![7u8; 3 << 20]
        );
        assert!(sftp.metadata(&dir.join("up")).is_err(), "moved away");

        // A panel on the server.
        let mut panel = crate::Panel::new(&local, crate::Filter::default()).unwrap();
        panel.go_remote(sftp.clone(), dir.clone()).unwrap();
        assert!(
            panel.location().starts_with("sftp://")
                && panel.location().ends_with(&*dir.to_string_lossy())
        );
        let at = |panel: &crate::Panel, name: &str| {
            panel
                .entries()
                .iter()
                .position(|e| e.display_name() == name)
                .unwrap()
        };
        panel.set_cursor(at(&panel, "dir"));
        assert!(panel.open().unwrap());
        assert_eq!(panel.path(), dir.join("dir"));
        panel.up().unwrap();
        assert_eq!(
            panel.current_entry().unwrap().display_name(),
            "dir",
            "back on the folder it left"
        );
        panel.new_folder("made/inner").unwrap();
        assert!(sftp.metadata(&dir.join("made/inner")).unwrap().is_dir);
        panel.delete(true).unwrap();
        assert!(
            sftp.metadata(&dir.join("made")).is_err(),
            "deleted, not to a trash"
        );
        assert!(panel.state().url.is_some());
        panel.disconnect(local.clone()).unwrap();
        assert!(panel.remote().is_none() && panel.location() == local.display().to_string());

        crate::vfs::create_all(&*sftp, &dir.join("a/b")).unwrap();
        sftp.rename(&dir.join("a"), &dir.join("c")).unwrap();
        crate::vfs::remove_all(&*sftp, &dir.join("c")).unwrap();
        assert!(sftp.metadata(&dir.join("c")).is_err());
        std::fs::remove_dir_all(&local).unwrap();
        drop(sftp);

        // Known now: the host key is not asked about again.
        let hosts = || {
            asked
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.starts_with("UnknownHost"))
                .count()
        };
        let before = hosts();
        Sftp::connect(&address, ask).unwrap();
        assert_eq!(hosts(), before);
    }
}
