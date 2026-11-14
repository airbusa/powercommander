//! FTP, and FTPS (explicit, AUTH TLS). A command and a transfer each take
//! a logged-in connection of their own from a pool. An FTPS certificate
//! is trusted as SSH host keys are: asked about once, then remembered.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use suppaftp::list::{File, ListParser, PosixPexQuery};
use suppaftp::types::FileType;
use suppaftp::{
    FtpError, ImplFtpStream, RustlsConnector, RustlsFtpStream, Status, TlsStream, TransferStream,
};

use crate::Entry;
use crate::remote::{self, Address, Ask, Prompt, Reply, Scheme};
use crate::vfs::{Fs, Meta, Remote};

type Stream = RustlsFtpStream;
/// suppaftp does not export its rustls stream type by name.
type Data = <Stream as Transfers>::Data;

trait Transfers {
    type Data;
}

impl<T: TlsStream> Transfers for ImplFtpStream<T> {
    type Data = TransferStream<T>;
}
type Pool = Arc<Mutex<Vec<Stream>>>;

const TIMEOUT: Duration = Duration::from_secs(30);
const KEPT: usize = 4;
const TRIES: usize = 3;
/// Certificates trusted for good, beside the settings.
const KNOWN: &str = "ftps_known_hosts";

/// How to open one more connection.
struct Server {
    host: String,
    port: u16,
    tls: Option<Arc<ClientConfig>>,
    user: String,
    password: String,
}

impl Server {
    fn session(&self) -> Result<Stream, FtpError> {
        let mut stream = self.dial()?;
        stream.login(&self.user, &self.password)?;
        prepare(&mut stream);
        Ok(stream)
    }

    fn dial(&self) -> Result<Stream, FtpError> {
        let address = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(FtpError::ConnectionError)?
            .next()
            .ok_or_else(|| FtpError::ConnectionError(io::ErrorKind::NotFound.into()))?;
        let tcp =
            TcpStream::connect_timeout(&address, TIMEOUT).map_err(FtpError::ConnectionError)?;
        let _ = tcp.set_read_timeout(Some(TIMEOUT * 4));
        let mut stream = Stream::connect_with_stream(tcp)?;
        // A server behind NAT may name its private address for data.
        stream.set_passive_nat_workaround(true);
        match &self.tls {
            Some(config) => stream.into_secure(RustlsConnector::from(config.clone()), &self.host),
            None => Ok(stream),
        }
    }
}

/// Binary transfers and UTF-8 names.
fn prepare(stream: &mut Stream) {
    let _ = stream.transfer_type(FileType::Binary);
    let _ = stream.opts("UTF8", Some("ON"));
}

pub struct Ftp {
    url: String,
    home: PathBuf,
    server: Server,
    idle: Pool,
    mlsd: bool,
    mlst: bool,
    mfmt: bool,
}

impl Ftp {
    /// Connects and logs in, asking `ask` what it must. Blocks: run it off
    /// the UI thread.
    pub fn connect(address: &Address, ask: Ask) -> io::Result<Arc<Self>> {
        let port = address.port.unwrap_or(21);
        let trust = (address.scheme == Scheme::Ftps)
            .then(|| Arc::new(Trust::new(&address.host, port, ask.clone())));
        let tls = trust.as_ref().map(|trust| {
            let provider = trust.provider.clone();
            let config = ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("TLS 1.2 and 1.3")
                .dangerous()
                .with_custom_certificate_verifier(trust.clone())
                .with_no_client_auth();
            Arc::new(config)
        });
        let anonymous = address.user.is_none();
        let mut server = Server {
            host: address.host.clone(),
            port,
            tls,
            user: address.user.clone().unwrap_or_else(|| "anonymous".into()),
            password: "anonymous@".into(),
        };
        let mut stream = server.dial().map_err(failed)?;
        for attempt in 1.. {
            if !anonymous {
                let text = match attempt {
                    1 => format!("Password for {}@{}:", server.user, server.host),
                    _ => format!(
                        "Wrong password. Password for {}@{}:",
                        server.user, server.host
                    ),
                };
                let prompt = Prompt::Secret {
                    title: "Password".into(),
                    text,
                    keyring: Some(remote::keyring_key(address)),
                };
                server.password = match ask(prompt) {
                    Reply::Secret(password) => password,
                    _ => return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")),
                };
            }
            match stream.login(&server.user, &server.password) {
                Ok(()) => break,
                Err(FtpError::UnexpectedResponse(response))
                    if response.status == Status::NotLoggedIn && !anonymous && attempt < TRIES =>
                {
                    // Some servers hang up after a wrong password.
                    if stream.noop().is_err() {
                        stream = server.dial().map_err(failed)?;
                    }
                }
                Err(err) => return Err(failed(err)),
            }
        }
        prepare(&mut stream);
        let features = stream.feat().unwrap_or_default();
        let has = |name: &str| features.keys().any(|key| key.eq_ignore_ascii_case(name));
        let home = stream.pwd().map_err(failed)?;
        // Trusted now for the session: nothing more to ask.
        if let Some(trust) = &trust {
            trust.ask.lock().unwrap().take();
        }
        let user = match anonymous {
            true => String::new(),
            false => format!("{}@", server.user),
        };
        let port = match port {
            21 => String::new(),
            port => format!(":{port}"),
        };
        Ok(Arc::new(Ftp {
            url: format!("{}://{user}{}{port}", address.scheme.name(), address.host),
            home: PathBuf::from(home),
            server,
            idle: Arc::new(Mutex::new(vec![stream])),
            mlsd: has("MLSD") || has("MLST"),
            mlst: has("MLST"),
            mfmt: has("MFMT"),
        }))
    }

    /// Where `address` points, else the login folder.
    pub fn start(&self, address: &Address) -> PathBuf {
        address
            .path
            .clone()
            .map_or_else(|| self.home.clone(), PathBuf::from)
    }

    /// Runs `work` on an idle connection, or a new one; a pooled one the
    /// server has dropped meanwhile is replaced once.
    fn attempt<T>(
        &self,
        work: impl Fn(&mut Stream) -> Result<T, FtpError>,
    ) -> io::Result<(Stream, T)> {
        let pooled = self.idle.lock().unwrap().pop();
        let stale = pooled.is_some();
        let mut stream = match pooled {
            Some(stream) => stream,
            None => self.server.session().map_err(failed)?,
        };
        let mut done = work(&mut stream);
        if stale && done.as_ref().is_err_and(broken) {
            stream = self.server.session().map_err(failed)?;
            done = work(&mut stream);
        }
        match done {
            Ok(value) => Ok((stream, value)),
            Err(err) => {
                if !broken(&err) {
                    keep(&self.idle, stream);
                }
                Err(failed(err))
            }
        }
    }

    fn with<T>(&self, work: impl Fn(&mut Stream) -> Result<T, FtpError>) -> io::Result<T> {
        let (stream, value) = self.attempt(work)?;
        keep(&self.idle, stream);
        Ok(value)
    }

    fn transfer(
        &self,
        work: impl Fn(&mut Stream) -> Result<Data, FtpError>,
    ) -> io::Result<Transfer> {
        let (control, data) = self.attempt(work)?;
        Ok(Transfer {
            control: Some(control),
            data: Some(data),
            idle: self.idle.clone(),
        })
    }

    /// A folder's files; a link to a folder is a folder.
    fn listing(&self, dir: &Path) -> io::Result<Vec<Listed>> {
        let dir = text(dir);
        self.with(|stream| {
            stream.cwd(&dir)?;
            let lines = match self.mlsd {
                true => stream.mlsd(None)?,
                false => stream.list(None)?,
            };
            let mut files: Vec<_> = lines
                .iter()
                .filter_map(|line| parse(line, self.mlsd))
                .collect();
            files.retain(|listed| listed.file.name() != "." && listed.file.name() != "..");
            for listed in &mut files {
                if listed.file.is_symlink() {
                    listed.is_dir = stream.cwd(join(&dir, listed.file.name())).is_ok();
                }
            }
            Ok(files)
        })
    }
}

fn keep(idle: &Pool, stream: Stream) {
    let mut idle = idle.lock().unwrap();
    if idle.len() < KEPT {
        idle.push(stream);
    }
}

/// The connection is gone, not merely refused.
fn broken(err: &FtpError) -> bool {
    match err {
        FtpError::ConnectionError(_) => true,
        FtpError::UnexpectedResponse(response) => response.status == Status::NotAvailable,
        _ => false,
    }
}

fn failed(err: FtpError) -> io::Error {
    let kind = match &err {
        FtpError::ConnectionError(err) => err.kind(),
        FtpError::UnexpectedResponse(response) => match response.status {
            Status::FileUnavailable => io::ErrorKind::NotFound,
            Status::NotLoggedIn => io::ErrorKind::PermissionDenied,
            _ => io::ErrorKind::Other,
        },
        _ => io::ErrorKind::Other,
    };
    let text = match &err {
        FtpError::UnexpectedResponse(response) => {
            let body = String::from_utf8_lossy(&response.body);
            body.trim().to_owned()
        }
        err => err.to_string(),
    };
    io::Error::new(kind, text)
}

struct Listed {
    file: File,
    /// Where the server tells it.
    mode: Option<u32>,
    is_dir: bool,
}

/// A listing line: MLSD, or `ls -l` and DOS style.
fn parse(line: &str, mlsd: bool) -> Option<Listed> {
    let (file, told) = match mlsd {
        true => (
            ListParser::parse_mlst(line).ok()?,
            line.to_ascii_lowercase().contains("unix.mode="),
        ),
        false => match ListParser::parse_posix(line) {
            Ok(file) => (file, true),
            Err(_) => (ListParser::parse_dos(line).ok()?, false),
        },
    };
    Some(Listed {
        mode: told.then(|| mode(&file)),
        is_dir: file.is_directory(),
        file,
    })
}

fn mode(file: &File) -> u32 {
    let whom = [
        PosixPexQuery::Owner,
        PosixPexQuery::Group,
        PosixPexQuery::Others,
    ];
    whom.into_iter().fold(0, |mode, who| {
        let bits = (file.can_read(who) as u32) << 2
            | (file.can_write(who) as u32) << 1
            | file.can_execute(who) as u32;
        mode << 3 | bits
    })
}

fn modified(file: &File) -> Option<SystemTime> {
    Some(file.modified()).filter(|time| *time > UNIX_EPOCH)
}

fn meta(listed: &Listed) -> Meta {
    Meta {
        is_dir: listed.is_dir,
        is_link: listed.file.is_symlink(),
        size: listed.file.size() as u64,
        modified: modified(&listed.file),
        mode: listed.mode,
    }
}

/// FTP paths are POSIX, whatever the local system's are.
fn text(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn join(dir: &str, name: &str) -> String {
    format!("{}/{name}", dir.trim_end_matches('/'))
}

impl Fs for Ftp {
    fn id(&self) -> String {
        self.url.clone()
    }

    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        let (Some(name), Some(parent)) = (path.file_name(), path.parent()) else {
            return Ok(Meta {
                is_dir: true,
                ..Meta::default()
            });
        };
        if self.mlst {
            let path = text(path);
            let line = self.with(|stream| stream.mlst(Some(&path)))?;
            let listed =
                parse(&line, true).ok_or_else(|| io::Error::other(format!("MLST: {line}")))?;
            return Ok(meta(&listed));
        }
        let found = self
            .listing(parent)?
            .into_iter()
            .find(|listed| listed.file.name() == name);
        Ok(meta(&found.ok_or(io::ErrorKind::NotFound)?))
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<OsString>> {
        let files = self.listing(dir)?;
        Ok(files
            .into_iter()
            .map(|listed| listed.file.name().into())
            .collect())
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let path = text(path);
        Ok(Box::new(
            self.transfer(|stream| stream.retr_as_stream(&path))?,
        ))
    }

    fn create(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let path = text(path);
        Ok(Box::new(
            self.transfer(|stream| stream.put_with_stream(&path))?,
        ))
    }

    /// REST, then RETR.
    fn open_at(&self, path: &Path, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        let path = text(path);
        let offset = usize::try_from(offset).map_err(io::Error::other)?;
        Ok(Box::new(self.transfer(|stream| {
            stream.resume_transfer(offset)?;
            // Refused, the connection goes back to the pool: without the offset.
            stream.retr_as_stream(&path).inspect_err(|_| {
                let _ = stream.resume_transfer(0);
            })
        })?))
    }

    /// APPE.
    fn append(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let path = text(path);
        Ok(Box::new(self.transfer(|stream| stream.append_with_stream(&path))?))
    }

    /// Where the server can: the time by MFMT, the mode by SITE CHMOD.
    fn set_meta(&self, path: &Path, meta: &Meta) -> io::Result<()> {
        let path = text(path);
        if self.mfmt
            && let Some(time) = meta.modified
        {
            let time = chrono::DateTime::<chrono::Utc>::from(time).format("%Y%m%d%H%M%S");
            let command = format!("MFMT {time} {path}");
            let _ = self.with(|stream| stream.custom_command(&command, &[Status::File]));
        }
        if let Some(mode) = meta.mode {
            let _ = self.with(|stream| stream.site(format!("CHMOD {mode:o} {path}")));
        }
        Ok(())
    }

    fn mkdir(&self, path: &Path) -> io::Result<()> {
        let path = text(path);
        self.with(|stream| stream.mkdir(&path))
    }

    fn remove(&self, path: &Path, is_dir: bool) -> io::Result<()> {
        let path = text(path);
        match is_dir {
            true => self.with(|stream| stream.rmdir(&path)),
            false => self.with(|stream| stream.rm(&path)),
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (from, to) = (text(from), text(to));
        self.with(|stream| stream.rename(&from, &to))
    }
}

impl Remote for Ftp {
    fn url(&self) -> String {
        self.url.clone()
    }

    fn home(&self) -> PathBuf {
        self.home.clone()
    }

    fn entries(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        let mut entries: Vec<Entry> = self
            .listing(dir)?
            .into_iter()
            .map(|Listed { file, mode, is_dir }| Entry {
                hidden: file.name().starts_with('.'),
                system: false,
                attributes: crate::sftp::mode_text(mode),
                tags: Vec::new(),
                online: false,
                is_dir,
                size: file.size() as u64,
                modified: modified(&file),
                name: file.name().into(),
            })
            .collect();
        if dir.parent().is_some() {
            entries.insert(0, crate::parent_entry(None));
        }
        Ok(entries)
    }
}

/// A file being read or written; when whole, the server's answer is read
/// and the connection goes back to the pool.
struct Transfer {
    control: Option<Stream>,
    data: Option<Data>,
    idle: Pool,
}

impl Transfer {
    fn finish(&mut self) -> io::Result<()> {
        let Some(data) = self.data.take() else {
            return Ok(());
        };
        data.finish().map_err(failed)?;
        if let Some(control) = self.control.take() {
            keep(&self.idle, control);
        }
        Ok(())
    }

    fn data(&mut self) -> io::Result<&mut Data> {
        self.data
            .as_mut()
            .ok_or_else(|| io::Error::other("the transfer is over"))
    }
}

impl Read for Transfer {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.data.is_none() {
            return Ok(0);
        }
        let read = self.data()?.read(buffer)?;
        if read == 0 && !buffer.is_empty() {
            self.finish()?;
        }
        Ok(read)
    }
}

impl Write for Transfer {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.data()?.write(buffer)
    }

    /// The end of an upload: whether the server took it all.
    fn flush(&mut self) -> io::Result<()> {
        if self.data.is_none() {
            return Ok(());
        }
        self.data()?.flush()?;
        self.finish()
    }
}

/// FTPS certificates: one known, or one the user trusts.
struct Trust {
    /// `host:port`, as the file of known ones keys it.
    server: String,
    provider: Arc<CryptoProvider>,
    ask: Mutex<Option<Ask>>,
    /// The certificate trusted this session.
    trusted: Mutex<Option<String>>,
}

impl std::fmt::Debug for Trust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Trust({})", self.server)
    }
}

impl Trust {
    fn new(host: &str, port: u16, ask: Ask) -> Self {
        Trust {
            server: format!("{host}:{port}"),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
            ask: Mutex::new(Some(ask)),
            trusted: Mutex::new(None),
        }
    }

    /// The server's fingerprints the file has, by line number.
    fn recorded(&self) -> Vec<(usize, String)> {
        let text = known_file().and_then(|file| std::fs::read_to_string(file).ok());
        let lines = text.unwrap_or_default();
        let ours = |(at, line): (usize, &str)| {
            let (server, fingerprint) = line.split_once(' ')?;
            (server == self.server).then(|| (at + 1, fingerprint.to_owned()))
        };
        lines.lines().enumerate().filter_map(ours).collect()
    }

    fn remember(&self, fingerprint: &str) -> io::Result<()> {
        let file = known_file().ok_or_else(|| io::Error::other("no configuration folder"))?;
        std::fs::create_dir_all(file.parent().unwrap_or(&file))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)?;
        writeln!(file, "{} {fingerprint}", self.server)
    }
}

fn known_file() -> Option<PathBuf> {
    Some(crate::settings::config_dir()?.join(KNOWN))
}

/// `SHA256:` and the certificate's digest in hex pairs.
fn fingerprint(certificate: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, certificate);
    let hex: Vec<String> = digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect();
    format!("SHA256:{}", hex.join(":"))
}

impl ServerCertVerifier for Trust {
    fn verify_server_cert(
        &self,
        certificate: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fingerprint = fingerprint(certificate);
        let mut trusted = self.trusted.lock().unwrap();
        let refused = || rustls::Error::General("the server's certificate is not trusted".into());
        match trusted.as_ref() {
            Some(known) if *known == fingerprint => return Ok(ServerCertVerified::assertion()),
            // Asked once, while connecting; never about a second certificate.
            Some(_) => return Err(refused()),
            None => {}
        }
        let recorded = self.recorded();
        if recorded.iter().any(|(_, known)| *known == fingerprint) {
            *trusted = Some(fingerprint);
            return Ok(ServerCertVerified::assertion());
        }
        if let Some((line, _)) = recorded.first() {
            let file = known_file().unwrap_or_default();
            return Err(rustls::Error::General(format!(
                "The TLS certificate of {} has CHANGED since line {line} of {} recorded it. \
                 Someone could be listening in; if the server has a new certificate, remove that line.",
                self.server,
                file.display()
            )));
        }
        let Some(ask) = self.ask.lock().unwrap().clone() else {
            return Err(refused());
        };
        let prompt = Prompt::UnknownHost {
            host: self.server.clone(),
            key_type: "TLS certificate".into(),
            fingerprint: fingerprint.clone(),
        };
        match ask(prompt) {
            Reply::Trust { remember } => {
                if remember {
                    // shortcut: a certificate not written down is asked about next time.
                    let _ = self.remember(&fingerprint);
                }
                *trusted = Some(fingerprint);
                Ok(ServerCertVerified::assertion())
            }
            _ => Err(refused()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.provider.signature_verification_algorithms;
        verify_tls12_signature(message, certificate, signed, algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.provider.signature_verification_algorithms;
        verify_tls13_signature(message, certificate, signed, algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_listings() {
        let file = parse(
            "type=file;size=6;modify=20261110120000;UNIX.mode=0640; hello world.txt",
            true,
        )
        .unwrap();
        assert_eq!(
            (file.file.name(), file.file.size(), file.mode),
            ("hello world.txt", 6, Some(0o640))
        );
        let dir = parse("type=dir;modify=20261110120000; pub", true).unwrap();
        assert!(dir.is_dir && dir.mode.is_none());
        let file = parse(
            "-rw-r--r--    1 1000     1000            6 Nov 10 12:00 notes.txt",
            false,
        )
        .unwrap();
        assert_eq!((file.file.name(), file.mode), ("notes.txt", Some(0o644)));
        let link = parse(
            "lrwxrwxrwx    1 0        0              8 Jan 01  2024 www -> /var/www",
            false,
        )
        .unwrap();
        assert!(link.file.is_symlink() && link.file.name() == "www");
        let dos = parse("11-10-26  12:00PM       <DIR>          Backup", false).unwrap();
        assert!(dos.is_dir && dos.file.name() == "Backup" && dos.mode.is_none());
        assert!(parse("total 12", false).is_none());
    }

    #[test]
    fn prints_fingerprints() {
        let print = fingerprint(b"certificate");
        assert!(
            print.starts_with("SHA256:") && print.len() == 7 + 32 * 3 - 1,
            "{print}"
        );
    }

    /// Needs a server: PC_FTP_TEST=ftp://user@host:port/dir (or ftps://)
    /// with PC_FTP_SECRET its password, `hello.txt` saying "hello\n" in
    /// `dir`, and a `dir` folder beside it.
    #[test]
    #[ignore]
    fn talks_to_a_server() {
        use crate::ops::{Kind, Operation, Overwrite, Queue, Status, Target};
        use crate::vfs::Local;
        let url = std::env::var("PC_FTP_TEST").expect("PC_FTP_TEST");
        let address = Address::parse(&url).unwrap();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let heard = asked.clone();
        let ask: Ask = Arc::new(move |prompt| {
            heard.lock().unwrap().push(format!("{prompt:?}"));
            match prompt {
                Prompt::UnknownHost { .. } => Reply::Trust { remember: true },
                Prompt::Secret { .. } => {
                    Reply::Secret(std::env::var("PC_FTP_SECRET").unwrap_or_default())
                }
            }
        });
        let ftp = Ftp::connect(&address, ask.clone()).unwrap();
        let dir = ftp.start(&address);
        crate::vfs::resumes_both_ways(&*ftp, &dir);
        let names: Vec<String> = ftp
            .entries(&dir)
            .unwrap()
            .iter()
            .map(|e| e.display_name())
            .collect();
        assert!(names.contains(&"hello.txt".to_string()), "{names:?}");
        let mut hello = String::new();
        ftp.open(&dir.join("hello.txt"))
            .unwrap()
            .read_to_string(&mut hello)
            .unwrap();
        assert_eq!(hello, "hello\n");
        assert_eq!(ftp.metadata(&dir.join("hello.txt")).unwrap().size, 6);
        assert!(ftp.metadata(&dir.join("dir")).unwrap().is_dir);
        assert_eq!(
            ftp.metadata(&dir.join("nothing")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );

        let local = std::env::temp_dir().join(format!("pc-ftp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&local);
        std::fs::create_dir_all(local.join("up/sub")).unwrap();
        std::fs::write(local.join("up/sub/big.bin"), vec![7u8; 3 << 20]).unwrap();
        std::fs::write(local.join("up/a.txt"), "a").unwrap();
        let remote: Arc<dyn Fs> = ftp.clone();
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
        assert_eq!(
            (up.status, up.progress.bytes),
            (Status::Done, (3 << 20) + 1)
        );
        assert_eq!(
            ftp.metadata(&dir.join("up/sub/big.bin")).unwrap().size,
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
        assert!(ftp.metadata(&dir.join("up")).is_err(), "moved away");

        // An archive on the server: browsed from a copy, `..` back on the server.
        std::fs::create_dir_all(local.join("pack/inner")).unwrap();
        std::fs::write(local.join("pack/inner/x.txt"), "x").unwrap();
        let status =
            crate::archive::seven_zip(&["a", "-bso0", "-tzip"], Some(&local.join("p.zip")))
                .arg("inner")
                .current_dir(local.join("pack"))
                .status()
                .unwrap();
        assert!(status.success());
        crate::vfs::upload(&local.join("p.zip"), &*ftp, &dir.join("p.zip")).unwrap();
        let mut panel = crate::Panel::new(&local, crate::Filter::default()).unwrap();
        panel.go_remote(ftp.clone(), dir.clone()).unwrap();
        let at = |panel: &crate::Panel, name: &str| {
            panel
                .entries()
                .iter()
                .position(|e| e.display_name() == name)
                .unwrap()
        };
        panel.set_cursor(at(&panel, "p.zip"));
        assert!(panel.open().unwrap());
        assert!(
            panel.location().starts_with(&ftp.url()) && panel.location().ends_with("p.zip"),
            "{}",
            panel.location()
        );
        assert_eq!(panel.folder(), dir, "the server's folder");
        assert!(panel.archive_origin().is_some());
        panel.up().unwrap();
        assert_eq!(
            (panel.path(), panel.current_entry().unwrap().display_name()),
            (dir.clone(), "p.zip".into())
        );
        ftp.remove(&dir.join("p.zip"), false).unwrap();

        crate::vfs::create_all(&*ftp, &dir.join("a/b")).unwrap();
        ftp.rename(&dir.join("a"), &dir.join("c")).unwrap();
        crate::vfs::remove_all(&*ftp, &dir.join("c")).unwrap();
        assert!(ftp.metadata(&dir.join("c")).is_err());
        std::fs::remove_dir_all(&local).unwrap();
        drop(ftp);

        // Known now: the certificate is not asked about again.
        let hosts = || {
            asked
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.starts_with("UnknownHost"))
                .count()
        };
        let before = hosts();
        Ftp::connect(&address, ask).unwrap();
        assert_eq!(hosts(), before);
    }
}
