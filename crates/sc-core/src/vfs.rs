//! What file operations read and write: the local disk and servers.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use crate::Entry;

/// A file being written is named so until it is whole; a broken copy's
/// stays, to go on from.
pub const PART: &str = ".sc-part";
const BUFFER: usize = 1 << 20;
/// How much of a part's end must match the source's there to go on from it.
const OVERLAP: u64 = 64 * 1024;

#[derive(Clone, Debug, Default)]
pub struct Meta {
    pub is_dir: bool,
    /// A link is never followed.
    pub is_link: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// Unix permission bits, where the file system has them.
    pub mode: Option<u32>,
}

pub trait Fs: Send + Sync {
    /// Two values with the same id are one file system, where a move is a rename.
    fn id(&self) -> String;
    fn metadata(&self, path: &Path) -> io::Result<Meta>;
    fn list(&self, dir: &Path) -> io::Result<Vec<OsString>>;
    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + Send>>;
    fn create(&self, path: &Path) -> io::Result<Box<dyn Write + Send>>;
    /// From byte `offset` on, to go on with a broken copy.
    fn open_at(&self, _path: &Path, _offset: u64) -> io::Result<Box<dyn Read + Send>> {
        Err(io::ErrorKind::Unsupported.into())
    }
    /// Written on at its end, to go on with a broken copy.
    fn append(&self, _path: &Path) -> io::Result<Box<dyn Write + Send>> {
        Err(io::ErrorKind::Unsupported.into())
    }
    /// After writing: the source's time and permissions.
    fn set_meta(&self, path: &Path, meta: &Meta) -> io::Result<()>;
    fn mkdir(&self, path: &Path) -> io::Result<()>;
    /// A file, a link, or an empty folder.
    fn remove(&self, path: &Path, is_dir: bool) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Copies a link as a link, where the file system can.
    fn copy_link(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// A server's file system, as a panel lists it.
pub trait Remote: Fs {
    /// `sftp://user@host`: titles show it, and typing it again reconnects.
    fn url(&self) -> String;
    /// Where a fresh connection starts.
    fn home(&self) -> PathBuf;
    /// `..` first, except at the root.
    fn entries(&self, dir: &Path) -> io::Result<Vec<Entry>>;
}

/// Streams `source` to `target` under a part name, renamed when whole, so
/// a failed copy leaves the target as it was. A broken copy's part stays and
/// the next copy goes on from it; a cancelled one's goes. `copied` hears of
/// each buffer, and of a part gone on from (true); its error stops the copy.
pub fn copy_file(
    from: &dyn Fs,
    source: &Path,
    to: &dyn Fs,
    target: &Path,
    meta: &Meta,
    copied: &mut dyn FnMut(u64, bool) -> io::Result<()>,
) -> io::Result<()> {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let part = target.with_file_name(format!("{name}{PART}"));
    let mut write = || -> io::Result<()> {
        let (mut reader, mut writer) = match resumable(from, source, to, &part, meta) {
            Some((reader, writer, done)) => {
                copied(done, true)?;
                (reader, writer)
            }
            None => (from.open(source)?, to.create(&part)?),
        };
        let mut buffer = vec![0; BUFFER];
        loop {
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            };
            writer.write_all(&buffer[..read])?;
            copied(read as u64, false)?;
        }
        writer.flush()?;
        drop(writer);
        if to.metadata(target).is_ok() {
            to.remove(target, false)?;
        }
        to.rename(&part, target)?;
        let _ = to.set_meta(target, meta);
        Ok(())
    };
    let written = write();
    if written.as_ref().is_err_and(|err| err.kind() == io::ErrorKind::Interrupted) {
        let _ = to.remove(&part, false);
    }
    written
}

/// A broken copy's part to go on from: shorter than the source, written
/// since the source last changed, its last bytes the same as the source's
/// there. The source reader is where the part ends.
type Resumed = (Box<dyn Read + Send>, Box<dyn Write + Send>, u64);

fn resumable(from: &dyn Fs, source: &Path, to: &dyn Fs, part: &Path, meta: &Meta) -> Option<Resumed> {
    let written = to.metadata(part).ok()?;
    let done = written.size;
    if done == 0 || done >= meta.size {
        return None;
    }
    // Changed since, maybe before where the part ends: from the beginning. In
    // whole seconds, as servers keep them.
    let seconds = |time: SystemTime| time.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |since| since.as_secs());
    if let (Some(changed), Some(written)) = (meta.modified, written.modified)
        && seconds(changed) > seconds(written)
    {
        return None;
    }
    let overlap = done.min(OVERLAP);
    let (mut theirs, mut ours) = (vec![0; overlap as usize], vec![0; overlap as usize]);
    let mut reader = from.open_at(source, done - overlap).ok()?;
    reader.read_exact(&mut theirs).ok()?;
    to.open_at(part, done - overlap).ok()?.read_exact(&mut ours).ok()?;
    if theirs != ours {
        return None;
    }
    Some((reader, to.append(part).ok()?, done))
}

/// A server's file in a temporary folder, for the viewer and editors.
pub fn fetch(fs: &dyn Fs, path: &Path) -> io::Result<PathBuf> {
    fetch_until(fs, path, &AtomicBool::new(false))
}

/// As `fetch`, given up once `stop` is set.
pub fn fetch_until(fs: &dyn Fs, path: &Path, stop: &AtomicBool) -> io::Result<PathBuf> {
    let meta = fs.metadata(path)?;
    let dir = crate::archive::fresh_scratch()?;
    let local = dir.join(path.file_name().unwrap_or_default());
    let copied = copy_file(fs, path, &Local, &local, &meta, &mut |_, _| match stop.load(Ordering::Relaxed) {
        true => Err(io::Error::new(io::ErrorKind::Interrupted, "stopped")),
        false => Ok(()),
    });
    if copied.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    copied.map(|()| local)
}

/// The edited temporary file back to the server.
pub fn upload(local: &Path, fs: &dyn Fs, path: &Path) -> io::Result<()> {
    let meta = Local.metadata(local)?;
    copy_file(&Local, local, fs, path, &meta, &mut |_, _| Ok(()))
}

/// Removes a folder and all in it, or a file.
pub fn remove_all(fs: &dyn Fs, path: &Path) -> io::Result<()> {
    let meta = fs.metadata(path)?;
    if meta.is_dir && !meta.is_link {
        for name in fs.list(path)? {
            remove_all(fs, &path.join(name))?;
        }
    }
    fs.remove(path, meta.is_dir && !meta.is_link)
}

/// Creates `path` and the folders above it that are missing.
pub fn create_all(fs: &dyn Fs, path: &Path) -> io::Result<()> {
    if fs.metadata(path).is_ok_and(|meta| meta.is_dir) {
        return Ok(());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        create_all(fs, parent)?;
    }
    fs.mkdir(path)
}

pub struct Local;

impl Fs for Local {
    fn id(&self) -> String {
        "local".into()
    }

    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        let meta = fs::symlink_metadata(path)?;
        #[cfg(unix)]
        let mode = Some(std::os::unix::fs::PermissionsExt::mode(&meta.permissions()));
        #[cfg(not(unix))]
        let mode = None;
        Ok(Meta {
            is_dir: meta.is_dir(),
            is_link: meta.is_symlink(),
            size: meta.len(),
            modified: meta.modified().ok(),
            mode,
        })
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<OsString>> {
        fs::read_dir(dir)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect()
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(File::open(path)?))
    }

    fn create(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(File::create(path)?))
    }

    fn open_at(&self, path: &Path, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(Box::new(file))
    }

    fn append(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(File::options().append(true).open(path)?))
    }

    fn set_meta(&self, path: &Path, meta: &Meta) -> io::Result<()> {
        if let Some(modified) = meta.modified {
            // Unix sets times through any handle, so folders and read-only files get them too.
            let file = match cfg!(unix) {
                true => File::open(path)?,
                false => File::options().write(true).open(path)?,
            };
            file.set_modified(modified)?;
        }
        #[cfg(unix)]
        if let Some(mode) = meta.mode {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    }

    fn mkdir(&self, path: &Path) -> io::Result<()> {
        fs::create_dir(path)
    }

    fn remove(&self, path: &Path, is_dir: bool) -> io::Result<()> {
        match is_dir {
            true => fs::remove_dir(path),
            false => fs::remove_file(path),
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    #[cfg(unix)]
    fn copy_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)
    }
}

/// A broken upload to `server` and a broken download from it each go on
/// from their part; for the servers' tests.
#[cfg(test)]
pub(crate) fn resumes_both_ways(server: &dyn Fs, dir: &Path) {
    let local = std::env::temp_dir().join(format!("sc-resume-server-{}", std::process::id()));
    fs::create_dir_all(&local).unwrap();
    let data: Vec<u8> = (0..3 * BUFFER + 123).map(|at| (at % 251) as u8).collect();
    let source = local.join("big.bin");
    fs::write(&source, &data).unwrap();
    let broken = |_: u64, _: bool| -> io::Result<()> { Err(io::ErrorKind::ConnectionReset.into()) };
    let mut heard = Vec::new();
    let up = dir.join("resume-up.bin");
    let meta = Local.metadata(&source).unwrap();
    assert!(copy_file(&Local, &source, server, &up, &meta, &mut { broken }).is_err());
    copy_file(&Local, &source, server, &up, &meta, &mut |bytes, _| Ok(heard.push(bytes))).unwrap();
    assert_eq!(heard[0], BUFFER as u64, "the upload went on");
    let down = local.join("down.bin");
    let meta = server.metadata(&up).unwrap();
    assert!(copy_file(server, &up, &Local, &down, &meta, &mut { broken }).is_err());
    // A server's reads come in smaller pieces than the buffer.
    let part = fs::metadata(local.join(format!("down.bin{PART}"))).unwrap().len();
    heard.clear();
    copy_file(server, &up, &Local, &down, &meta, &mut |bytes, _| Ok(heard.push(bytes))).unwrap();
    assert!(part > 0 && heard[0] == part, "the download went on from {part}: {heard:?}");
    assert_eq!(fs::read(&down).unwrap(), data);
    server.remove(&up, false).unwrap();
    fs::remove_dir_all(&local).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_broken_copy_goes_on_and_a_cancelled_one_leaves_nothing() {
        let dir = std::env::temp_dir().join(format!("sc-resume-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (source, target) = (dir.join("big.bin"), dir.join("copy.bin"));
        let part = dir.join(format!("copy.bin{PART}"));
        let data: Vec<u8> = (0..3 * BUFFER + 123).map(|at| (at % 251) as u8).collect();
        fs::write(&source, &data).unwrap();
        let meta = Local.metadata(&source).unwrap();
        let fail = |kind: io::ErrorKind| move |_: u64, _: bool| -> io::Result<()> { Err(kind.into()) };

        // Broken after the first buffer: the part stays, and the next copy goes on from it.
        assert!(copy_file(&Local, &source, &Local, &target, &meta, &mut fail(io::ErrorKind::ConnectionReset)).is_err());
        assert_eq!(fs::metadata(&part).unwrap().len(), BUFFER as u64);
        let mut heard = Vec::new();
        copy_file(&Local, &source, &Local, &target, &meta, &mut |bytes, resumed| Ok(heard.push((bytes, resumed)))).unwrap();
        assert_eq!(heard[0], (BUFFER as u64, true));
        assert_eq!(heard.iter().map(|(bytes, _)| bytes).sum::<u64>(), data.len() as u64);
        assert_eq!(fs::read(&target).unwrap(), data);
        assert!(!part.exists());

        // A part that is not this source's start: from the beginning.
        fs::write(&part, vec![7u8; 1000]).unwrap();
        copy_file(&Local, &source, &Local, &target, &meta, &mut |_, _| Ok(())).unwrap();
        assert_eq!(fs::read(&target).unwrap(), data);

        // The source changed since the part was written, though its end matches: from the beginning.
        assert!(copy_file(&Local, &source, &Local, &target, &meta, &mut fail(io::ErrorKind::ConnectionReset)).is_err());
        let mut changed = data.clone();
        changed[0] = 255;
        fs::write(&source, &changed).unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        File::options().write(true).open(&source).unwrap().set_modified(later).unwrap();
        let meta = Local.metadata(&source).unwrap();
        let mut heard = Vec::new();
        copy_file(&Local, &source, &Local, &target, &meta, &mut |_, resumed| Ok(heard.push(resumed))).unwrap();
        assert!(!heard[0]);
        assert_eq!(fs::read(&target).unwrap(), changed);

        // Cancelled: no part left.
        assert!(copy_file(&Local, &source, &Local, &target, &meta, &mut fail(io::ErrorKind::Interrupted)).is_err());
        assert!(!part.exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
