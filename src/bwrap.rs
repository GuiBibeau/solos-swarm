//! One bubblewrap command line, flag by flag, in the order the calls are made.
//!
//! The run path builds its sandboxes with it: the Pi session
//! (`PiHarness::command`), `run_code` (`code.rs`) and `doctor`'s probe. The
//! frozen `mvp` path keeps its own hand-built lines in `sandbox.rs` and
//! `wasi_worker.rs`.
use std::ffi::{OsStr, OsString};

/// bwrap's options so far. [`Bwrap::exec`] ends the line with the program the
/// sandbox runs and gives back the argument vector, without bwrap itself.
/// No `Debug`: a `setenv` value can be a bearer token.
#[derive(Default)]
#[must_use]
pub struct Bwrap {
    args: Vec<OsString>,
}

impl Bwrap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn unshare_all(self) -> Self {
        self.arg("--unshare-all")
    }

    pub fn die_with_parent(self) -> Self {
        self.arg("--die-with-parent")
    }

    pub fn new_session(self) -> Self {
        self.arg("--new-session")
    }

    pub fn ro_bind(self, source: impl AsRef<OsStr>, dest: impl AsRef<OsStr>) -> Self {
        self.arg("--ro-bind").arg(source).arg(dest)
    }

    /// `--ro-bind-try`: bwrap skips it when `source` is missing.
    pub fn ro_bind_try(self, source: impl AsRef<OsStr>, dest: impl AsRef<OsStr>) -> Self {
        self.arg("--ro-bind-try").arg(source).arg(dest)
    }

    pub fn bind(self, source: impl AsRef<OsStr>, dest: impl AsRef<OsStr>) -> Self {
        self.arg("--bind").arg(source).arg(dest)
    }

    pub fn dev(self, dest: impl AsRef<OsStr>) -> Self {
        self.arg("--dev").arg(dest)
    }

    pub fn proc(self, dest: impl AsRef<OsStr>) -> Self {
        self.arg("--proc").arg(dest)
    }

    pub fn tmpfs(self, dest: impl AsRef<OsStr>) -> Self {
        self.arg("--tmpfs").arg(dest)
    }

    /// `--size BYTES --tmpfs DEST`: bwrap reads `--size` as part of the
    /// mount that follows it, so the two are one call.
    pub fn tmpfs_sized(self, bytes: u64, dest: impl AsRef<OsStr>) -> Self {
        self.arg("--size").arg(bytes.to_string()).tmpfs(dest)
    }

    pub fn dir(self, dest: impl AsRef<OsStr>) -> Self {
        self.arg("--dir").arg(dest)
    }

    /// `--symlink TARGET LINK`: `link` in the sandbox points at `target`.
    pub fn symlink(self, target: impl AsRef<OsStr>, link: impl AsRef<OsStr>) -> Self {
        self.arg("--symlink").arg(target).arg(link)
    }

    pub fn clearenv(self) -> Self {
        self.arg("--clearenv")
    }

    pub fn setenv(self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.arg("--setenv").arg(name).arg(value)
    }

    pub fn chdir(self, dir: impl AsRef<OsStr>) -> Self {
        self.arg("--chdir").arg(dir)
    }

    /// The whole argument vector: the options, then `program` and `args`.
    pub fn exec<I>(self, program: impl AsRef<OsStr>, args: I) -> Vec<OsString>
    where
        I: IntoIterator,
        I::Item: AsRef<OsStr>,
    {
        let mut line = self.arg(program).args;
        line.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        line
    }

    fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::Bwrap;
    use std::{
        ffi::{OsStr, OsString},
        os::unix::ffi::OsStrExt,
        path::{Path, PathBuf},
    };

    fn text(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|arg| arg.to_str().unwrap()).collect()
    }

    #[test]
    fn each_call_adds_its_flag_and_operands_in_call_order() {
        let args = Bwrap::new()
            .unshare_all()
            .die_with_parent()
            .new_session()
            .ro_bind("/usr", "/usr")
            .ro_bind_try("/etc", "/etc")
            .bind("/host/scratch", "/work")
            .dev("/dev")
            .proc("/proc")
            .tmpfs("/tmp")
            .tmpfs_sized(4096, "/scratch")
            .dir("/runtime")
            .symlink("usr/bin", "/bin")
            .clearenv()
            .setenv("HOME", "/work")
            .setenv("EMPTY", "")
            .chdir("/work")
            .exec("/usr/bin/bash", ["-c", "echo ok"]);
        assert_eq!(
            text(&args),
            [
                "--unshare-all",
                "--die-with-parent",
                "--new-session",
                "--ro-bind",
                "/usr",
                "/usr",
                "--ro-bind-try",
                "/etc",
                "/etc",
                "--bind",
                "/host/scratch",
                "/work",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--tmpfs",
                "/tmp",
                "--size",
                "4096",
                "--tmpfs",
                "/scratch",
                "--dir",
                "/runtime",
                "--symlink",
                "usr/bin",
                "/bin",
                "--clearenv",
                "--setenv",
                "HOME",
                "/work",
                "--setenv",
                "EMPTY",
                "",
                "--chdir",
                "/work",
                "/usr/bin/bash",
                "-c",
                "echo ok",
            ]
        );
    }

    #[test]
    fn the_program_ends_the_line_with_no_separator_and_args_may_be_none() {
        let args = Bwrap::new()
            .unshare_all()
            .exec("/usr/bin/true", Vec::<&str>::new());
        assert_eq!(text(&args), ["--unshare-all", "/usr/bin/true"]);
        assert_eq!(text(&Bwrap::new().exec("/bin/x", ["a"])), ["/bin/x", "a"]);
    }

    #[test]
    fn operands_keep_their_bytes_whatever_type_they_come_as() {
        let odd = OsStr::from_bytes(b"/srv/\xff\xfe");
        let owned = PathBuf::from("/opt/node");
        let args = Bwrap::new()
            .ro_bind(odd, "/odd")
            .ro_bind(&owned, String::from("/runtime/node"))
            .ro_bind(Path::new("/a b"), OsString::from("/c"))
            .exec(owned.join("bin/node"), [OsString::from("--x")]);
        assert_eq!(args.len(), 11);
        assert_eq!(args[1], odd);
        assert_eq!(args[4], OsStr::new("/opt/node"));
        assert_eq!(args[5], OsStr::new("/runtime/node"));
        assert_eq!(args[7], OsStr::new("/a b"));
        assert_eq!(args[9], OsStr::new("/opt/node/bin/node"));
        assert_eq!(args[10], OsStr::new("--x"));
    }
}
