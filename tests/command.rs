/*!
Defines a simple command snapshotting mechanism.

This took some inspiration from `insta-cmd`, but re-works a few things. In
particular, this provides a rudimentary "pipelining" abstraction that enables
one to pipe the stdout of one command into the stdin of another. This is
especially useful for bttf, since it heavily relies on composition.

This also defines a wrapper around `std::process::Command` that all of the
tests use instead. It's essentially the same builder with some helper methods
and, crucially, uses a owned builder instead of a mutable builder. This makes
it compose more nicely at the expense of allocs (which we do not care about in
tests).

I specifically wrote this in a way that it has no other dependencies on other
modules in this crate. That means it should be very easy to copy & paste to
other test suites.
*/

use std::{
    collections::BTreeMap,
    env::consts::EXE_SUFFIX,
    ffi::{OsStr, OsString},
    io::Write,
    path::{Path, PathBuf},
    process, thread,
};

use bstr::{BStr, BString, ByteSlice, ByteVec};

macro_rules! run_and_snapshot {
    ($cmd:expr, $body:expr) => {{
        let snap = $cmd.snapshot();
        let mut settings = insta::Settings::clone_current();
        settings.set_info(snap.info());
        settings.set_omit_expression(true);
        settings.bind(|| ($body)(snap.snapshot()));
    }};
}

macro_rules! assert_cmd_snapshot {
    ($spawnable:expr, @$snapshot:literal $(,)?) => {{
        $crate::command::run_and_snapshot!($spawnable, |snapshot: &str| {
            insta::assert_snapshot!(snapshot, @$snapshot);
        });
    }};
    ($name:expr, $spawnable:expr $(,)?) => {{
        $crate::command::run_and_snapshot!($spawnable, |snapshot: &str| {
            insta::assert_snapshot!($name, snapshot);
        });
    }};
    ($spawnable:expr $(,)?) => {{
        $crate::command::run_and_snapshot!($spawnable, |snapshot: &str| {
            insta::assert_snapshot!(snapshot);
        });
    }};
}

pub(crate) use {assert_cmd_snapshot, run_and_snapshot};

/// A snapshot generated from running a command.
///
/// This also comes with some contextual info that is shown in the `cargo insta
/// review` user interface, but is not actually included in the snapshot.
pub struct Snapshot {
    /// The contextual info put into the `cargo insta review` user interface.
    info: CommandInfo,
    /// The actual snapshot contents.
    snapshot: String,
    /// The raw `stdout` of the command.
    stdout: BString,
}

impl Snapshot {
    /// Creates a new snapshot from a wrapped command and the process output.
    fn new(cmd: &Command, output: &process::Output) -> Snapshot {
        let info = cmd.info();
        let snapshot = format!(
            "success: {:?}\n\
             exit_code: {}\n\
             ----- stdout -----\n\
             {}\n\
             ----- stderr -----\n\
             {}",
            output.status.success(),
            output.status.code().unwrap_or(!0),
            bytes_to_string(&output.stdout),
            bytes_to_string(&output.stderr),
        );
        let stdout = BString::from(output.stdout.as_bstr());
        Snapshot { info, snapshot, stdout }
    }

    /// Returns the Insta "info" that contextualizes the snapshot.
    pub fn info(&self) -> &CommandInfo {
        &self.info
    }

    /// Returns the snapshot derived from running the command.
    pub fn snapshot(&self) -> &str {
        &self.snapshot
    }

    /// Returns the raw stdout of the command that was run.
    pub fn stdout(&self) -> &BStr {
        self.stdout.as_bstr()
    }
}

/// A representation of a rudimentary pipeline of commands.
#[derive(Debug)]
pub struct Pipeline {
    /// When present, this gets piped into the first command.
    stdin: Option<BString>,
    /// The commands to execute in a pipeline. The stdout of each command is
    /// piped into the stdin of the subsequent command.
    priors: Vec<Command>,
    /// The last command to execute. A pipeline has to always have at least
    /// one command. When new commands are added, the current `last` is moved
    /// to the end of `priors` and the new command is made `last`.
    last: Command,
}

impl Pipeline {
    /// Create a pipe from the current last command's stdout to stdin for the
    /// given command.
    pub fn pipe(mut self, cmd: Command) -> Pipeline {
        self.add(cmd);
        self
    }

    /// Passes the provided bytes as stdin into the first process in this
    /// pipeline.
    pub fn pass_stdin(self, bytes: impl Into<Vec<u8>>) -> Pipeline {
        Pipeline { stdin: Some(BString::from(bytes.into())), ..self }
    }

    fn add(&mut self, cmd: Command) {
        let previous_last = std::mem::replace(&mut self.last, cmd);
        self.priors.push(previous_last);
    }

    /// Run the commands in this pipeline and create a snapshot from the output
    /// of the last command.
    pub fn snapshot(&self) -> Snapshot {
        let mut thread_stdin = None;
        let mut threads = vec![];
        let mut first_stdin = self.stdin.clone();
        let mut prior_stdout: Option<process::ChildStdout> = None;
        for cmd in self.priors.iter() {
            let mut cmd = cmd.std();
            if first_stdin.is_some() {
                cmd.stdin(process::Stdio::piped());
            } else if let Some(prior_stdout) = prior_stdout.take() {
                cmd.stdin(prior_stdout);
            } else {
                cmd.stdin(process::Stdio::null());
            }
            cmd.stdout(process::Stdio::piped());
            cmd.stderr(process::Stdio::piped());
            let mut child = cmd.spawn().unwrap();
            if let Some(first_stdin) = first_stdin.take() {
                let mut child_stdin = child.stdin.take().unwrap();
                thread_stdin = Some(thread::spawn(move || {
                    child_stdin.write_all(&first_stdin)
                }));
            }
            prior_stdout = child.stdout.take();
            threads.push((
                format!("{cmd:?}"),
                thread::spawn(move || child.wait_with_output()),
            ));
        }

        let mut last = self.last.std();
        if first_stdin.is_some() {
            last.stdin(process::Stdio::piped());
        } else if let Some(prior_stdout) = prior_stdout.take() {
            last.stdin(prior_stdout);
        } else {
            last.stdin(process::Stdio::null());
        }
        last.stdout(process::Stdio::piped());
        last.stderr(process::Stdio::piped());
        let output = match first_stdin.take() {
            None => {
                let output = last.output().unwrap();
                output
            }
            Some(first_stdin) => {
                let mut child = last.spawn().unwrap();
                let mut child_stdin = child.stdin.take().unwrap();
                thread_stdin = Some(thread::spawn(move || {
                    child_stdin.write_all(&first_stdin)
                }));
                child.wait_with_output().unwrap()
            }
        };
        let mut snap = Snapshot::new(&self.last, &output);

        for (cmd_debug, thread) in threads {
            let output = thread.join().unwrap().unwrap();
            if !output.status.success() {
                panic!(
                    "command `{cmd_debug}` failed with exit code {exit},\n\
                     ----- stderr -----\n\
                     {stderr}",
                    exit = output.status.code().unwrap_or(!0),
                    stderr = bytes_to_string(&output.stderr),
                );
            }
        }
        if let Some(thread_stdin) = thread_stdin {
            thread_stdin.join().unwrap().unwrap();
            snap.info.set_stdin(self.stdin.as_ref().unwrap());
        }
        snap
    }
}

impl From<Command> for Pipeline {
    fn from(cmd: Command) -> Pipeline {
        Pipeline { stdin: None, priors: vec![], last: cmd }
    }
}

/// An unfortunate wrapper around `std::process::Command`.
///
/// This basically exposes the same behavior API, except it returns `Command`
/// instead of `&mut Command`. Notably though, the `stdin`, `stdout` and
/// `stderr` methods are not available here, since they can represent I/O
/// resources. If callers need to set them, they should create a
/// `std::process::Command` first and then set them. But if you're using the
/// snapshotting and pipeline infrastructure defined above, then you shouldn't
/// need to futz with these things in most tests anyway.
///
/// This probably results in more allocs in some cases, but we don't care.
/// We're using this in tests. And this is way more convenient. Otherwise,
/// things like `Pipeline` become super annoying because it really wants to
/// own the commands, but building commands using `std::process::Command`
/// strongly biases toward `&mut std::process::Command`. A `Pipeline` could
/// store `&mut std::process::Command`, but I theorized this wouldn't work well
/// because lifetimes on mutable borrows are invariant. (I didn't even bother
/// trying it.)
///
/// Note that we really only wrap the command "builder" API. We don't wrap the
/// various output types like `Child` and `Output` and so on. (Thank goodness.)
/// And we still use `std::process::Stdio` directly.
#[derive(Clone, Debug)]
pub struct Command {
    bin: OsString,
    current_dir: Option<PathBuf>,
    args: Vec<OsString>,
    envs: Vec<EnvAction>,
}

impl Command {
    /// Create a new command wrapper for the given binary program.
    pub fn new(bin: impl AsRef<OsStr>) -> Command {
        Command {
            bin: bin.as_ref().to_os_string(),
            current_dir: None,
            args: vec![],
            envs: vec![],
        }
    }

    /// Add an argument to the end of this command invocation.
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Command {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    /// Add arguments to the end of this command invocation.
    pub fn args(
        mut self,
        args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    ) -> Command {
        for arg in args {
            self = self.arg(arg);
        }
        self
    }

    /// Set an environment variable.
    pub fn env(
        mut self,
        key: impl AsRef<OsStr>,
        val: impl AsRef<OsStr>,
    ) -> Command {
        self.envs.push(EnvAction::Set(
            key.as_ref().to_os_string(),
            val.as_ref().to_os_string(),
        ));
        self
    }

    /// Set zero or more environment variables.
    #[expect(dead_code)]
    pub fn envs(
        mut self,
        vars: impl IntoIterator<Item = (impl AsRef<OsStr>, impl AsRef<OsStr>)>,
    ) -> Command {
        for (key, val) in vars {
            self = self.env(key, val);
        }
        self
    }

    /// Remove an environment variable (also prevents inheriting from the
    /// parent process).
    #[expect(dead_code)]
    pub fn env_remove(mut self, key: impl AsRef<OsStr>) -> Command {
        self.envs.push(EnvAction::Remove(key.as_ref().to_os_string()));
        self
    }

    /// Clear all previously set environment variables.
    #[expect(dead_code)]
    pub fn env_clear(mut self) -> Command {
        self.envs.push(EnvAction::Clear);
        self
    }

    /// Set the current directory in which to run this command.
    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Command {
        self.current_dir = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Returns a new pipeline where the given data is passed into the stdin
    /// of this command.
    pub fn stdin(self, stdin: impl Into<Vec<u8>>) -> Pipeline {
        Pipeline::from(self).pass_stdin(stdin)
    }

    /// Returns a new pipeline with this command's stdout connected to the
    /// stdin of the given command.
    pub fn pipe(self, cmd: Command) -> Pipeline {
        let mut pipeline = Pipeline::from(self);
        pipeline.add(cmd);
        pipeline
    }

    /// Turn this wrapper into a fresh `std::process::Command`.
    pub fn std(&self) -> process::Command {
        let mut cmd = process::Command::new(&self.bin);
        if let Some(ref current_dir) = self.current_dir {
            cmd.current_dir(current_dir);
        }
        cmd.args(self.args.iter());
        for action in self.envs.iter() {
            match *action {
                EnvAction::Set(ref key, ref val) => {
                    cmd.env(key, val);
                }
                EnvAction::Remove(ref key) => {
                    cmd.env_remove(key);
                }
                EnvAction::Clear => {
                    cmd.env_clear();
                }
            }
        }
        cmd
    }

    /// Runs this command and returns a snapshot based on its output.
    pub fn snapshot(&self) -> Snapshot {
        let output = self.std().output().unwrap();
        Snapshot::new(self, &output)
    }

    /// Returns the info for this command.
    pub fn info(&self) -> CommandInfo {
        // This is a little silly, but it means we only need to write the
        // `CommandInfo` constructor once for one universal type.
        CommandInfo::new(&self.std())
    }
}

/// An action to take on environment variables.
#[derive(Clone, Debug)]
enum EnvAction {
    /// Maps to `std::process::Command::env`.
    Set(OsString, OsString),
    /// Maps to `std::process::Command::env_remove`.
    Remove(OsString),
    /// Maps to `std::process::Command::env_clear`.
    Clear,
}

/// Information about a particular command.
///
/// This is fed into `insta` as contextual information that doesn't appear
/// directly in the snapshot, but instead in the `cargo insta review` user
/// interface.
#[derive(Clone, Debug, serde::Serialize)]
pub struct CommandInfo {
    bin: String,
    args: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdin: Option<String>,
}

impl CommandInfo {
    fn new(cmd: &process::Command) -> CommandInfo {
        let bin = {
            let path = Path::new(cmd.get_program())
                .file_name()
                .unwrap_or(OsStr::new("{UNKNOWN}"));
            let mut bin =
                <[u8]>::from_os_str(path).expect("valid UTF-8 on Windows");
            // Kinda surprised bstr doesn't have strip_{prefix,suffix}. Maybe
            // we should add it?
            if bin.ends_with_str(EXE_SUFFIX) {
                bin = &bin[..bin.len() - EXE_SUFFIX.len()];
            }
            bin
        };
        CommandInfo {
            bin: bytes_to_string(&bin),
            args: cmd.get_args().map(os_str_to_string).collect(),
            env: cmd
                .get_envs()
                .map(|(k, v)| {
                    (
                        os_str_to_string(k),
                        os_str_to_string(v.unwrap_or(OsStr::new(""))),
                    )
                })
                .collect(),
            stdin: None,
        }
    }

    fn set_stdin(&mut self, bytes: &[u8]) {
        self.stdin = Some(bytes_to_string(bytes));
    }
}

/// Return a command prepared to execute the binary with the given name.
///
/// This may have more than just a binary name. For example, this tries to
/// detect `cross` and setup a runner for it.
pub fn bin(name: &str) -> Command {
    let bin = bin_path(name);
    match cross_runner() {
        None => Command::new(bin),
        Some(runner) => Command::new(runner).arg(bin),
    }
}

/// Returns a path to the Cargo project binary with the given name.
fn bin_path(name: &str) -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_bttf").map(PathBuf::from).unwrap_or_else(
        || {
            std::env::current_exe()
                .expect("test executable's current directory")
                .parent()
                .expect("test executable's directory")
                .parent()
                .expect("target profile directory")
                .join(format!("{name}{}", EXE_SUFFIX))
        },
    )
}

fn cross_runner() -> Option<String> {
    let runner = std::env::var("CROSS_RUNNER").ok()?;
    if runner.is_empty() || runner == "empty" {
        return None;
    }
    if cfg!(target_arch = "powerpc64") {
        Some("qemu-ppc64".to_string())
    } else if cfg!(target_arch = "x86") {
        Some("i386".to_string())
    } else {
        // Make a guess... Sigh.
        Some(format!("qemu-{}", std::env::consts::ARCH))
    }
}

/// Turns a slice of bytes into a human readable string.
///
/// When the bytes are valid UTF-8, they are returned as-is. Otherwise, they
/// are escaped into valid UTF-8 using bstr's escaping mechanism.
fn bytes_to_string(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(string) => string.to_string(),
        Err(_) => bytes.escape_bytes().to_string(),
    }
}

/// Like `bytes_to_string`, but starts with an OS string.
///
/// On Windows, if `os_str` is not valid UTF-8, then lossy UTF-8 decoding is
/// done.
fn os_str_to_string(os_str: &OsStr) -> String {
    bytes_to_string(&Vec::from_os_str_lossy(os_str))
}
