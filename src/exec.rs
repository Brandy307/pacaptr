use crate::print::*;
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use futures::{Stream, StreamExt, TryStreamExt};
pub use is_root::is_root;
use lazy_static::lazy_static;
use regex::Regex;
use std::ffi::OsStr;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::process::Command as Exec;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::try_join;
use tokio_util::codec::{BytesCodec, FramedRead};

/// Different ways in which a command shall be dealt with.
#[derive(Copy, Clone, Debug)]
pub enum Mode {
    /// Solely print out the command that should be executed, and stop.
    PrintCmd,

    /// Silently collect all the `stdout`/`stderr` combined. Print nothing.
    Mute,

    /// Print out the command which should be executed, run it and collect its `stdout`/`stderr` combined.
    /// Potentially dangerous as it destroys the colored `stdout`. Use it only if really necessary.
    CheckAll,

    /// Print out the command which should be executed, run it and collect its `stderr`.
    /// This will work with a colored `stdout`.
    CheckErr,

    /// A CUSTOM prompt implemented by `pacaptr`.
    /// Like `CheckErr`, but will ask for confirmation before proceeding.
    Prompt,
}

pub type StatusCode = i32;

/// Representation of what a command returns.
#[derive(Debug, Clone)]
pub struct Output {
    /// The captured `stdout`, sometimes mixed with captured `stderr`.
    pub contents: Vec<u8>,
    /// `Some(n)` for exit code, `None` for signals.
    pub code: Option<StatusCode>,
}

impl Default for Output {
    fn default() -> Self {
        Output {
            contents: Default::default(),
            code: Some(0),
        }
    }
}

/// A command to be executed, provided in `command-keywords-flags` form.  
/// For example, `[brew install]-[curl fish]-[--dry-run]`).
#[derive(Debug, Clone, Default)]
pub struct Cmd<S = String> {
    pub sudo: bool,
    pub cmd: Vec<S>,
    pub kws: Vec<S>,
    pub flags: Vec<S>,
}

impl<S> Cmd<S> {
    /// Determine if this command needs to run with `sudo -S`.
    pub fn needs_sudo(&self) -> bool {
        self.sudo && !is_root()
    }
}

impl Cmd<String> {
    pub fn new(cmd: &[&str]) -> Self {
        Self {
            cmd: cmd.iter().map(|&s| s.to_owned()).collect(),
            ..Default::default()
        }
    }

    pub fn new_sudo(cmd: &[&str]) -> Self {
        Self::new(cmd).sudo(true)
    }

    pub fn kws(mut self, kws: &[&str]) -> Self {
        self.kws = kws.iter().map(|&s| s.to_owned()).collect();
        self
    }

    pub fn flags(mut self, flags: &[&str]) -> Self {
        self.flags = flags.iter().map(|&s| s.to_owned()).collect();
        self
    }

    pub fn sudo(mut self, sudo: bool) -> Self {
        self.sudo = sudo;
        self
    }
}

impl<S: AsRef<OsStr>> Cmd<S> {
    /// Convert a `Cmd` object into a `subprocess::Exec`.
    pub fn build(self) -> Exec {
        // * We use `sudo -S` to launch subprocess if `sudo` is `true` and the current user is not `root`.
        // ! Special fix for `zypper`: `zypper install -y curl` is accepted,
        // ! but not `zypper install curl -y`.
        // ! So we place the flags first, and then keywords.
        if self.needs_sudo() {
            let mut builder = Exec::new("sudo");
            builder
                .arg("-S")
                .args(&self.cmd)
                .args(&self.flags)
                .args(&self.kws);
            builder
        } else {
            let (cmd, subcmd) = self
                .cmd
                .split_first()
                .expect("Failed to build Cmd, command is empty");
            let mut builder = Exec::new(cmd);
            builder.args(subcmd).args(&self.flags).args(&self.kws);
            builder
        }
    }
}

/// Helper macro to implement `exec_checkerr` and `exec_checkall`.
/// Take contents from the input stream `$src` and copy to `$out1` and `$out2`,
/// and finally return `$out2`.
/// The boolean `$mute1` decides whether to mute `$out1`.
macro_rules! exec_tee {
    ( $src:expr, $out1:expr, $out2:expr, $mute1:expr  ) => {{
        while let Some(mb) = $src.next().await {
            let b = mb?;
            let b = b.as_ref();
            if $mute1 {
                $out2.write_all(b).await?;
            } else {
                try_join!($out1.write_all(b), $out2.write_all(b))?;
            }
        }

        $out2
    }};
}

impl<S: AsRef<OsStr> + AsRef<str>> Cmd<S> {
    /// Execute a command and return a `Result<Vec<u8>, _>`.  
    /// The exact behavior depends on the `mode` passed in.  
    /// See `exec::Mode`'s documentation for more info.
    pub async fn exec(self, mode: Mode) -> Result<Output> {
        match mode {
            Mode::PrintCmd => {
                print_cmd(&self, PROMPT_CANCELED);
                Ok(Default::default())
            }
            Mode::Mute => self.exec_checkall(true).await,
            Mode::CheckAll => {
                print_cmd(&self, PROMPT_RUN);
                self.exec_checkall(false).await
            }
            Mode::CheckErr => {
                print_cmd(&self, PROMPT_RUN);
                self.exec_checkerr(false).await
            }
            Mode::Prompt => self.exec_prompt(false).await,
        }
    }

    /// Execute a command and return its `stdout` and `stderr`.
    /// If `mute` is `false`, then its normal `stdout/stderr` will be printed in the console too.
    async fn exec_checkall(self, mute: bool) -> Result<Output> {
        let mut child = self
            .build()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn child process")?;
        let stdout_reader = child
            .stdout
            .take()
            .map(into_bytes)
            .ok_or_else(|| anyhow!("Child process did not have a handle to stdout"))?;
        let stderr_reader = child
            .stderr
            .take()
            .map(into_bytes)
            .ok_or_else(|| anyhow!("Child process did not have a handle to stderr"))?;
        let mut merged_reader = tokio::stream::StreamExt::merge(stdout_reader, stderr_reader);

        let code: JoinHandle<Result<Option<i32>>> = tokio::spawn(async move {
            let status = child
                .wait()
                .await
                .context("Child process encountered an error")?;
            Ok(status.code())
        });

        let mut out = Vec::<u8>::new();
        let mut stdout = tokio::io::stdout();
        let contents = exec_tee!(merged_reader, stdout, out, mute);

        Ok(Output {
            contents,
            code: code.await.unwrap()?,
        })
    }

    /// Execute a command and collect its `stderr`.  
    /// If `mute` is `false`, then its normal `stderr` will be printed in the console too.
    async fn exec_checkerr(self, mute: bool) -> Result<Output> {
        let mut child = self
            .build()
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn child process")?;
        let mut stderr_reader = child
            .stderr
            .take()
            .map(into_bytes)
            .ok_or_else(|| anyhow!("Child did not have a handle to stderr"))?;

        let code: JoinHandle<Result<Option<i32>>> = tokio::spawn(async move {
            let status = child
                .wait()
                .await
                .context("Child process encountered an error")?;
            Ok(status.code())
        });

        let mut out = Vec::<u8>::new();
        let mut stderr = tokio::io::stderr();
        let contents = exec_tee!(stderr_reader, stderr, out, mute);

        Ok(Output {
            contents,
            code: code.await.unwrap()?,
        })
    }

    /// Execute a command and collect its `stderr`.
    /// If `mute` is `false`, then its normal `stderr` will be printed in the console too.
    /// The user will be prompted if (s)he wishes to continue with the command execution.
    #[allow(clippy::mutex_atomic)]
    async fn exec_prompt(self, mute: bool) -> Result<Output> {
        lazy_static! {
            static ref ALL_YES: Mutex<bool> = Mutex::new(false);
        }

        let mut all_yes = ALL_YES.lock().await;
        let proceed: bool = if *all_yes {
            true
        } else {
            print_cmd(&self, PROMPT_PENDING);
            match tokio::task::block_in_place(move || {
                prompt(
                    "Proceed",
                    "[Yes/all/no]",
                    &["", "y", "yes", "a", "all", "n", "no"],
                    false,
                )
                .to_lowercase()
            })
            .as_ref()
            {
                // The default answer is `Yes`
                "y" | "yes" | "" => true,
                // You can also say `All` to answer `Yes` to all the other questions that follow.
                "a" | "all" => {
                    *all_yes = true;
                    true
                }
                // Or you can say `No`.
                "n" | "no" => false,
                _ => unreachable!(),
            }
        };
        if !proceed {
            return Ok(Default::default());
        }
        print_cmd(&self, PROMPT_RUN);
        self.exec_checkerr(mute).await
    }
}

impl<S: AsRef<str>> std::fmt::Display for Cmd<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sudo_prefix: &str = if self.needs_sudo() { "sudo -S " } else { "" };
        let mut res = sudo_prefix.to_owned();
        let cmd_str = self
            .cmd
            .iter()
            .chain(&self.kws)
            .chain(&self.flags)
            .map(|s| s.as_ref())
            .collect::<Vec<&str>>()
            .join(" ");
        res.push_str(&cmd_str);
        write!(f, "{}", res)
    }
}

/// Prompt and get the output string.
/// This action won't end until an expected answer is found.
/// If `case_sensitive == false`, then `expected` should be all lower case patterns.
pub fn prompt(question: &str, options: &str, expected: &[&str], case_sensitive: bool) -> String {
    use std::io::Write;
    loop {
        let mut answer = String::new();
        print_question(question, options);
        let _ = std::io::stdout().flush();
        std::io::stdin()
            .read_line(&mut answer)
            .expect("Error while reading user input");
        if !case_sensitive {
            answer = answer.to_lowercase();
        }
        if let Some('\n') = answer.chars().next_back() {
            answer.pop();
        }
        if let Some('\r') = answer.chars().next_back() {
            answer.pop();
        }
        if expected.iter().any(|&x| x == answer) {
            break answer;
        }
    }
}

/// Find all lines in the given `text` that matches all the `patterns`.
pub fn grep(text: &str, patterns: &[&str]) -> Vec<String> {
    let rs: Vec<Regex> = patterns
        .iter()
        .map(|&pat| Regex::new(pat).unwrap())
        .collect();
    text.lines()
        .filter(|&line| rs.iter().all(|regex| regex.is_match(line)))
        .map(|s| s.to_owned())
        .collect()
}

/// Check if an executable exists by name (consult `$PATH`) or by path.
/// To check by one parameter only, pass `""` as another.
pub fn is_exe(name: &str, path: &str) -> bool {
    (!path.is_empty() && std::path::Path::new(path).exists())
        || (!name.is_empty() && which::which(name).is_ok())
}

/// Helper function to turn an `AsyncRead` to a `Stream`.
// See also: https://stackoverflow.com/a/59327560
pub fn into_bytes<R: AsyncRead>(r: R) -> impl Stream<Item = tokio::io::Result<Bytes>> {
    FramedRead::new(r, BytesCodec::new()).map_ok(|bytes| bytes.freeze())
}

/*
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::test;

    #[test]
    async fn simple_run() {
        println!("Starting!");
        let cmd = Cmd::new(&["bash", "-c"])
            .kws(&[r#"printf "Hello\n"; sleep 3; printf "World\n"; sleep 3; printf "!\n""#]);
        let res = cmd.exec_checkall(false).await.unwrap();
        dbg!(res);
    }
}
*/
