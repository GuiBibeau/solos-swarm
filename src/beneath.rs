//! Files beneath a directory that a sandbox can also write. The sandbox can
//! put a link anywhere in it, and the host writes as itself, so a path is
//! never resolved by name: it is walked one component at a time from a
//! descriptor of the directory opened once, each directory opened with
//! `O_NOFOLLOW` relative to the one before and a missing one made with
//! `mkdirat`. A file is written under a fresh name with `O_CREAT | O_EXCL`
//! and moved over its own with `renameat`, which never follows a link either.
//! So neither a link, `..` nor a swap made during the call carries a write
//! outside. `run_code`'s `files` and a worker's `write_file` and `edit_file`
//! all write through here.

use std::ffi::{CStr, CString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::Path;

/// The longest path taken, in bytes.
pub const MAX_PATH_BYTES: usize = 1024;
/// A directory made on the way, as `run_code` always made them.
const DIR_MODE: libc::mode_t = 0o700;
/// A new file's mode, less the umask.
const FILE_MODE: libc::c_uint = 0o666;
/// The permission bits a replaced file keeps.
const PERMISSIONS: libc::mode_t = 0o777;

/// Why a path was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Not a relative path of plain names: empty, absolute, too long, or with
    /// `.`, `..`, an empty name or a NUL in it.
    Invalid(String),
    /// A link on the way, or the file itself is one.
    Link(String),
    Missing(String),
    /// A name on the way is not a directory, or the file is not a regular one.
    NotFile(String),
    /// Larger than the caller takes.
    TooLarge(String),
    /// Not UTF-8.
    NotText(String),
    /// An I/O error of this host.
    Host(String),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (Self::Invalid(why)
        | Self::Link(why)
        | Self::Missing(why)
        | Self::NotFile(why)
        | Self::TooLarge(why)
        | Self::NotText(why)
        | Self::Host(why)) = self;
        f.write_str(why)
    }
}

impl std::error::Error for Refused {}

/// What a write did.
#[derive(Debug, PartialEq, Eq)]
pub struct Written {
    /// No file was there before.
    pub created: bool,
}

/// A directory, opened once, that every path is resolved beneath.
#[derive(Debug)]
pub struct Beneath {
    root: OwnedFd,
    /// What the directory is called in a refusal, such as `/work`.
    shown: &'static str,
}

impl Beneath {
    /// `dir`, which the host chose, so it may be reached through a link. It
    /// is called `shown` in what a refusal says.
    pub fn open(dir: &Path, shown: &'static str) -> io::Result<Self> {
        let file = File::open(dir)?;
        if !file.metadata()?.is_dir() {
            return Err(io::Error::other(format!(
                "{} is not a directory",
                dir.display()
            )));
        }
        Ok(Self {
            root: file.into(),
            shown,
        })
    }

    /// Write `content` to `path`, making the directories above it, in place
    /// of a regular file there, whose permissions it keeps.
    pub fn write(&self, path: &str, content: &[u8]) -> Result<Written, Refused> {
        let (dirs, name) = self.split(path)?;
        let dir = self.walk(&dirs, true)?;
        let shown = self.shown(path);
        let mode = match stat_at(dir.as_fd(), &name) {
            Ok(stat) => Some(regular(&stat, &shown)? & PERMISSIONS),
            Err(err) if err.raw_os_error() == Some(libc::ENOENT) => None,
            Err(err) => return Err(Refused::Host(format!("cannot look at {shown}: {err}"))),
        };
        replace(dir.as_fd(), &name, content, mode).map_err(|err| match err.raw_os_error() {
            Some(libc::EISDIR) => Refused::NotFile(format!("{shown} is a directory")),
            _ => Refused::Host(format!("cannot write {shown}: {err}")),
        })?;
        Ok(Written {
            created: mode.is_none(),
        })
    }

    /// The text of the regular file at `path`, at most `limit` bytes of UTF-8.
    pub fn read_text(&self, path: &str, limit: usize) -> Result<String, Refused> {
        let (dirs, name) = self.split(path)?;
        let dir = self.walk(&dirs, false)?;
        let shown = self.shown(path);
        // O_NONBLOCK: a FIFO there does not hold the open waiting for a writer.
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY;
        let file = match open_at(dir.as_fd(), &name, flags, 0) {
            Ok(fd) => File::from(fd),
            Err(err) => return Err(why(dir.as_fd(), &name, &shown, &err)),
        };
        let failed = |err: io::Error| Refused::Host(format!("cannot read {shown}: {err}"));
        if !file.metadata().map_err(failed)?.is_file() {
            return Err(Refused::NotFile(format!("{shown} is not a regular file")));
        }
        let mut bytes = Vec::new();
        file.take((limit as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(failed)?;
        if bytes.len() > limit {
            return Err(Refused::TooLarge(format!(
                "{shown} is larger than {limit} bytes"
            )));
        }
        String::from_utf8(bytes).map_err(|_| Refused::NotText(format!("{shown} is not UTF-8 text")))
    }

    fn shown(&self, path: &str) -> String {
        format!("{}/{path}", self.shown)
    }

    /// The directories above `path`'s file, and its file's name.
    fn split(&self, path: &str) -> Result<(Vec<CString>, CString), Refused> {
        let invalid = |why: String| Err(Refused::Invalid(why));
        if path.is_empty() {
            return invalid("the path is empty".into());
        }
        if path.len() > MAX_PATH_BYTES {
            return invalid(format!("the path is longer than {MAX_PATH_BYTES} bytes"));
        }
        if path.contains('\0') {
            return invalid("a path holds no NUL".into());
        }
        if path.starts_with('/') {
            return invalid(format!(
                "{path} is absolute: give a path beneath {}",
                self.shown
            ));
        }
        let mut names = Vec::new();
        for name in path.split('/') {
            if matches!(name, "" | "." | "..") {
                return invalid(format!(
                    "{path} has an empty, . or .. part: give a plain path beneath {}",
                    self.shown
                ));
            }
            names.push(CString::new(name).expect("no NUL, checked above"));
        }
        let file = names.pop().expect("a path that is not empty has a name");
        Ok((names, file))
    }

    /// The directory `dirs` lead to, each opened beneath the one before
    /// without following a link, and made where it is missing if `make`.
    fn walk(&self, dirs: &[CString], make: bool) -> Result<OwnedFd, Refused> {
        let mut dir = self
            .root
            .try_clone()
            .map_err(|err| Refused::Host(format!("cannot open {}: {err}", self.shown)))?;
        let mut shown = self.shown.to_owned();
        for name in dirs {
            shown.push('/');
            shown.push_str(&name.to_string_lossy());
            let opened = match open_dir(dir.as_fd(), name) {
                Err(err) if make && err.raw_os_error() == Some(libc::ENOENT) => {
                    match mkdir_at(dir.as_fd(), name) {
                        Err(err) if err.raw_os_error() != Some(libc::EEXIST) => {
                            return Err(Refused::Host(format!("cannot make {shown}: {err}")));
                        }
                        _ => open_dir(dir.as_fd(), name),
                    }
                }
                opened => opened,
            };
            dir = opened.map_err(|err| why(dir.as_fd(), name, &shown, &err))?;
        }
        Ok(dir)
    }
}

/// What a failed open of `name` in `dir` means, after a look at what is there.
fn why(dir: BorrowedFd<'_>, name: &CStr, shown: &str, err: &io::Error) -> Refused {
    if err.raw_os_error() == Some(libc::ENOENT) {
        return Refused::Missing(format!("{shown} does not exist"));
    }
    match stat_at(dir, name) {
        Ok(stat) if kind(&stat) == libc::S_IFLNK => link(shown),
        Ok(stat) if kind(&stat) != libc::S_IFDIR && err.raw_os_error() == Some(libc::ENOTDIR) => {
            Refused::NotFile(format!("{shown} is not a directory"))
        }
        _ => Refused::Host(format!("cannot open {shown}: {err}")),
    }
}

/// The mode of a regular file, or why what is there is refused.
fn regular(stat: &libc::stat, shown: &str) -> Result<libc::mode_t, Refused> {
    match kind(stat) {
        libc::S_IFREG => Ok(stat.st_mode),
        libc::S_IFLNK => Err(link(shown)),
        libc::S_IFDIR => Err(Refused::NotFile(format!("{shown} is a directory"))),
        _ => Err(Refused::NotFile(format!("{shown} is not a regular file"))),
    }
}

fn link(shown: &str) -> Refused {
    Refused::Link(format!(
        "{shown} is a link, and a path through a link or to one is refused: use the path it \
         leads to"
    ))
}

fn kind(stat: &libc::stat) -> libc::mode_t {
    stat.st_mode & libc::S_IFMT
}

/// Write `content` under a fresh name in `dir` and move it over `name`. With
/// `mode`, the new file has it; else it is a new file's.
fn replace(
    dir: BorrowedFd<'_>,
    name: &CStr,
    content: &[u8],
    mode: Option<libc::mode_t>,
) -> io::Result<()> {
    let fresh = CString::new(format!(".solos-write-{}", uuid::Uuid::new_v4().simple()))
        .expect("no NUL in a uuid");
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW;
    let file = File::from(open_at(dir, &fresh, flags, FILE_MODE)?);
    let written = fill(file, content, mode).and_then(|()| rename_at(dir, &fresh, name));
    if written.is_err() {
        let _ = unlink_at(dir, &fresh);
    }
    written
}

/// Write `content` to `file` and close it, with `mode` first if given.
fn fill(mut file: File, content: &[u8], mode: Option<libc::mode_t>) -> io::Result<()> {
    if let Some(mode) = mode {
        change_mode(&file, mode)?;
    }
    file.write_all(content)
}

fn last<T: PartialOrd + Default>(ret: T) -> io::Result<T> {
    if ret < T::default() {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

fn open_at(
    dir: BorrowedFd<'_>,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> io::Result<OwnedFd> {
    // SAFETY: dir is an open descriptor borrowed for the call and name a
    // NUL-terminated string that outlives it; mode is the variadic argument
    // O_CREAT reads, already promoted to an unsigned int.
    let fd = last(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode,
        )
    })?;
    // SAFETY: openat returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn open_dir(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    open_at(dir, name, flags, 0)
}

fn mkdir_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<()> {
    // SAFETY: dir is an open descriptor borrowed for the call and name a
    // NUL-terminated string that outlives it.
    last(unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), DIR_MODE) }).map(|_| ())
}

fn stat_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<libc::stat> {
    // SAFETY: stat is a plain C struct of integers, for which all zeroes is
    // a valid value.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: dir is an open descriptor borrowed for the call, name a
    // NUL-terminated string and stat a struct the call may write, all alive
    // for it.
    last(unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &raw mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    Ok(stat)
}

fn change_mode(file: &File, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: file is an open descriptor this function borrows for the call.
    last(unsafe { libc::fchmod(file.as_raw_fd(), mode) }).map(|_| ())
}

fn rename_at(dir: BorrowedFd<'_>, from: &CStr, to: &CStr) -> io::Result<()> {
    // SAFETY: dir is an open descriptor borrowed for the call, and from and
    // to NUL-terminated strings that outlive it.
    last(unsafe { libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr()) })
        .map(|_| ())
}

fn unlink_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<()> {
    // SAFETY: dir is an open descriptor borrowed for the call and name a
    // NUL-terminated string that outlives it.
    last(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) }).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{Beneath, Refused, Written};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    fn dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("beneath-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("work")).unwrap();
        dir
    }

    #[test]
    fn a_write_makes_its_directories_and_replaces_a_file_keeping_its_mode() {
        let dir = dir();
        let work = Beneath::open(&dir.join("work"), "/work").unwrap();
        assert_eq!(
            work.write("a/b/c.txt", b"one"),
            Ok(Written { created: true })
        );
        let file = dir.join("work/a/b/c.txt");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o751)).unwrap();
        assert_eq!(
            work.write("a/b/c.txt", b"two"),
            Ok(Written { created: false })
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), "two");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o751
        );
        assert_eq!(work.read_text("a/b/c.txt", 3), Ok("two".to_owned()));
        assert!(matches!(
            work.read_text("a/b/c.txt", 2),
            Err(Refused::TooLarge(_))
        ));
        let names: Vec<_> = fs::read_dir(dir.join("work/a/b")).unwrap().collect();
        assert_eq!(names.len(), 1, "no temporary file is left");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_path_that_is_not_plain_names_beneath_it_is_refused() {
        let dir = dir();
        let work = Beneath::open(&dir.join("work"), "/work").unwrap();
        for path in [
            "",
            "/etc/passwd",
            "../escape",
            "a/../../escape",
            "./a",
            "a//b",
            "a/",
            "a\0b",
        ] {
            assert!(
                matches!(work.write(path, b"x"), Err(Refused::Invalid(_))),
                "{path:?}"
            );
        }
        let long = "a/".repeat(600) + "b";
        assert!(matches!(work.write(&long, b"x"), Err(Refused::Invalid(_))));
        assert_eq!(fs::read_dir(dir.join("work")).unwrap().count(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_link_on_the_way_or_at_the_end_is_refused_and_nothing_is_written_outside() {
        let dir = dir();
        let outside = dir.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret"), "keep").unwrap();
        symlink(&outside, dir.join("work/escape")).unwrap();
        symlink("..", dir.join("work/up")).unwrap();
        symlink(outside.join("secret"), dir.join("work/secret")).unwrap();
        let work = Beneath::open(&dir.join("work"), "/work").unwrap();
        for path in [
            "escape/secret",
            "escape/new",
            "escape/sub/new",
            "up/outside/secret",
            "secret",
        ] {
            assert!(
                matches!(work.write(path, b"pwned"), Err(Refused::Link(_))),
                "write {path}"
            );
            assert!(
                matches!(work.read_text(path, 100), Err(Refused::Link(_))),
                "read {path}"
            );
        }
        assert_eq!(fs::read_to_string(outside.join("secret")).unwrap(), "keep");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_directory_or_a_file_on_the_way_is_not_a_file_to_write() {
        let dir = dir();
        fs::create_dir_all(dir.join("work/sub")).unwrap();
        fs::write(dir.join("work/plain"), "x").unwrap();
        let work = Beneath::open(&dir.join("work"), "/work").unwrap();
        assert!(matches!(work.write("sub", b"x"), Err(Refused::NotFile(_))));
        assert!(matches!(
            work.write("plain/below", b"x"),
            Err(Refused::NotFile(_))
        ));
        assert!(matches!(
            work.read_text("missing/file", 10),
            Err(Refused::Missing(_))
        ));
        assert!(matches!(
            work.read_text("missing", 10),
            Err(Refused::Missing(_))
        ));
        fs::remove_dir_all(&dir).unwrap();
    }
}
