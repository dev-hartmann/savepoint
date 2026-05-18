#![expect(clippy::as_conversions)]
#![expect(unused)]
#![allow(clippy::missing_const_for_fn)]
use std::env::{current_dir, set_current_dir};
use std::ffi::OsStr;
use std::fs::{self, read_to_string, remove_file, write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use clap::Parser;
use color_eyre::Section;
use color_eyre::eyre::{self, Result};
use colored::{ColoredString, Colorize};
use command_run::{Command, Error, Output};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use spinners::{Spinner, Spinners};
use uuid::Uuid;

use crate::eyre::eyre;

static ERRFILE: &str = ".checkpoint.error";
static STATEFILE: &str = ".checkpoint.start";

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Start autosave watch loop on a savepoint subbranch
    Start {
        /// Filename extension to watch (eg rs, js, py, java)
        #[arg(short, long, value_name = "filetype")]
        filetype: String,
        /// Command to run (use after -- if your shell requires it)
        command: Vec<String>,
        /// Don't run git commit when tests pass
        #[arg(short, long)]
        dryrun: bool,
        /// Clear screen between runs
        #[arg(short, long)]
        clear: bool,
        /// Don't display test output
        #[arg(short, long)]
        quiet: bool,
    },
    /// Stop autosave watch loop and merge
    Finalize {
        #[arg(short, long)]
        message: Option<String>,
        /// Don't run git merge or commit when tests pass
        #[arg(short, long)]
        dryrun: bool,
    },
}

/// State diagram:
/// ```mermaid
/// flowchart LR
/// PASSING-->|fail|FAILING
/// FAILING-->|pass; git commit|PASSING
/// ```
/// Other transitions are no-ops (such as tests passing while in passing state)
#[derive(Debug, Copy, Clone)]
struct SavePoint<'a> {
    program: &'a str,
    args: &'a [String],
    state: State,
}
#[derive(Debug, PartialEq, Clone, Copy)]
enum State {
    Passing,
    Failing,
}
#[allow(clippy::enum_glob_use)]
use State::*;

//TODO: All flags should get saved into self in new()
impl<'a> SavePoint<'a> {
    /// If error file exists, failing, if not, passing
    fn new(program: &'a str, args: &'a [String]) -> Self {
        let state = match fs::exists(ERRFILE) {
            Ok(_) => Passing,
            Err(_) => Failing,
        };
        Self {
            program,
            args,
            state,
        }
    }

    /// main state dispatcher
    fn test(mut self, program: &str, dryrun: bool, quiet: bool) -> Result<Self> {
        let res = if quiet {
            let mut sp = Spinner::new(Spinners::Line, format!("Running {program}..."));
            let res = cmdr(self.program, self.args, quiet);
            sp.stop();
            res
        } else {
            cmdr(self.program, self.args, quiet)
        };
        println!("done!");
        match (&self, res) {
            // noop
            (Self { state: Passing, .. }, Ok(_)) => Ok(self),
            (
                Self {
                    state: Failing | Passing,
                    ..
                },
                Err(_),
            ) => Ok(self.fail()),
            // notify, git commit
            (Self { state: Failing, .. }, Ok(_)) => self.pass(dryrun),
        }
    }

    /// fixed all errors, git commit
    fn pass(self, dryrun: bool) -> Result<Self> {
        commit("SAVEPOINT REACHED!", dryrun)?;
        rm_errfile()?;
        Ok(Self {
            state: Passing,
            ..self
        })
    }

    /// test just failed
    fn fail(self) -> Self {
        log(&"Error!".red().bold());
        let _ = create_errfile();
        Self {
            state: Failing,
            ..self
        }
    }
}

/// Clear ansi terminal and put cursor at top-left
fn clear() {
    print!("{esc}[2J{esc}[1;1H", esc = 27 as char);
}

fn log(message: &ColoredString) {
    let prefix = "🏁 CHECKPOINT: ".blue().bold();
    print!("{prefix}");
    println!("{message}");
}

fn start(filetype: &str, command: &[String], dryrun: bool, clear: bool, quiet: bool) -> Result<()> {
    //INFO: Ensure that if dryrun is not active, that the current environment
    //      includes the git command
    if !dryrun {
        // Check wether git is available.
        is_git_available()?;
        // Check if we are running within a git repo.
        is_git_repo()?;
    }

    let program = command
        .first()
        .ok_or_else(|| eyre!("Missing argument: COMMAND"))?;
    let args = command.get(1..).ok_or_else(|| eyre!("no program arg"))?;

    // Get current working branch as ref for later
    let starting_branch = current_branch()?;
    create_statefile(&starting_branch)?;

    // Switch to savepoint sub-branch
    branch(None, dryrun)?;

    // Install Ctrl-C handler to gracefully exit
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::Relaxed);
    })?;

    //INFO: File Watcher
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(Path::new("."), RecursiveMode::Recursive)?;
    let mut machine = SavePoint::new(program, args);

    //INFO: Main UI Loop
    while running.load(Ordering::Relaxed) {
        log(&"Monitoring...".white().bold());
        machine = machine.test(program, dryrun, quiet)?;
        if !running.load(Ordering::Relaxed) {
            break;
        }
        blockforfile(&rx, filetype, &running);
        if clear {
            crate::clear();
        }
    }

    log(
        &"Savepoint stopped. Consider running `savepoint finalize` to merge auto-commits."
            .yellow()
            .bold(),
    );
    Ok(())
}

fn finalize(message: Option<String>, dryrun: bool) -> Result<()> {
    if !dryrun {
        // Check wether git is available.
        is_git_available()?;
        // Check if we are running within a git repo.
        is_git_repo()?;
    }

    let starting_branch = read_statefile()?;
    merge_squashed(&starting_branch, message, dryrun)?;
    if !dryrun {
        let _ = rm_errfile();
        rm_statefile()?;
    }
    log(
        &format!("Finalized! Savepoints squashed into {starting_branch}.")
            .green()
            .bold(),
    );
    Ok(())
}

#[expect(clippy::result_large_err)]
fn cmdr(program: &str, args: &[String], quiet: bool) -> Result<Output, Error> {
    let mut command = Command::with_args(program, args);
    if quiet {
        let command = command.enable_capture();
        command.combine_output = true;
    }
    command.log_command = false;
    command.run()
}

fn main() -> Result<()> {
    // INFO: Setup
    color_eyre::install()?;

    let cli = Cli::parse();

    match cli.command {
        Commands::Start {
            filetype,
            command,
            dryrun,
            clear,
            quiet,
        } => {
            start(&filetype, &command, dryrun, clear, quiet)?;
        }
        Commands::Finalize { message, dryrun } => finalize(message, dryrun)?,
    }

    Ok(())
}

fn blockforfile(
    rx: &Receiver<Result<Event, notify::Error>>,
    extension: &str,
    running: &AtomicBool,
) {
    loop {
        // check if SIGINT has been sent and return from loop to stop
        if !running.load(Ordering::Relaxed) {
            return;
        }
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(Ok(Event {
                kind: EventKind::Modify(_),
                paths,
                ..
            })) if paths.first().map(|p| p.extension()) == Some(Some(OsStr::new(extension))) => {
                break;
            }
            _ => {
                // ignoring
            }
        }
    }
    while rx.recv_timeout(Duration::from_millis(100)).is_ok() {
        // DRAIN THE CHANNEL
    }
}

fn is_git_available() -> Result<()> {
    // We check that git exists by running git --version
    let mut git_version_command = Command::with_args("git", ["--version"]);
    git_version_command.log_command = false;
    git_version_command
        .enable_capture()
        .run()
        .map(|_| ())
        .map_err(|e| {
            if let command_run::ErrorKind::Run(run_error) = &e.kind
                && run_error.kind() == std::io::ErrorKind::NotFound
            {
                // git was not found
                return eyre!("could not find `git` command");
            }
            // Another error occured
            eyre!(
                "checking for `git` command failed with unexpected error {}",
                e
            )
        })
}

fn is_git_repo() -> Result<()> {
    let mut rev_parse = Command::with_args("git", ["rev-parse", "--is-inside-work-tree"]);
    rev_parse.log_command = false;
    rev_parse.enable_capture().run().map(|_| ()).map_err(|_| {
        eyre!("Current directory is not a git repository, consider running 'git init'")
    })
}

fn current_branch() -> Result<String> {
    let mut command = Command::with_args("git", ["rev-parse", "--abbrev-ref", "HEAD"]);
    command.log_command = false;
    command.capture = true;
    command
        .run()
        .map(|output| output.stdout_string_lossy().trim().to_string())
        .map_err(|_| {
            eyre!("Git command error.")
                .with_suggestion(|| "Check if current directory is a git repository")
        })
}

fn branch(name: Option<&str>, dryrun: bool) -> Result<String> {
    let branch_name = name.map_or_else(|| format!("savepoint/{}", Uuid::new_v4()), String::from);

    let log_msg = format!("Switching to branch {branch_name}!");

    let mut cmd_args = vec!["checkout"];
    if name.is_none() {
        cmd_args.push("-b");
    }
    cmd_args.push(&branch_name);

    if dryrun {
        log(&format!("(dry run) {log_msg}").green().bold());
        return Ok(branch_name);
    }
    log(&log_msg.green().bold());

    let mut command = Command::with_args("git", cmd_args);
    command.log_command = false;
    command.run().map(|_| branch_name).map_err(|_| {
        eyre!("Git command error.")
            .with_suggestion(|| "Check if current directory is a git repository")
    })
}

fn merge_squashed(starting_branch: &str, msg: Option<String>, dryrun: bool) -> Result<()> {
    let savepoint_branch = current_branch()?;
    let log_msg =
        format!("Merging squashed savepoints from {savepoint_branch} to {starting_branch}!");
    if dryrun {
        log(&format!("(dry run) {log_msg}").green().bold());
        return Ok(());
    }
    log(&log_msg.green().bold());

    // git checkout starting branch
    branch(Some(starting_branch), dryrun)?;

    // merge squashed commits from savepoint branch
    let mut merge = Command::with_args("git", ["merge", "--squash", &savepoint_branch]);
    merge.log_command = false;
    merge.run().map_err(|_| {
        eyre!("Squashing commits failed.")
            .with_suggestion(|| "Resolve conflicts manually and commit.")
    })?;

    // Cannot use commit fn here, since we don't use '-a'
    // and error message is different.
    // Uses optional provided `msg` or creates one.
    let message =
        msg.unwrap_or_else(|| format!("Savepoint: changes from {savepoint_branch} integrated!"));

    let mut commit = Command::with_args("git", ["commit", "-m", &message]);
    commit.log_command = false;
    commit.run().map_err(|_| {
        eyre!("Git commit failed.")
            .with_suggestion(|| "Check that the squash produced staged changes.")
    })?;

    Ok(())
}

fn commit(msg: &str, dryrun: bool) -> Result<()> {
    if dryrun {
        log(&"(dry run) Autosaving!".green().bold());
        return Ok(());
    }
    log(&"Autosaving!".green().bold());
    let mut command = Command::with_args("git", ["commit", "-am", msg]);
    command.log_command = false;
    if command.run().is_ok() {
        Ok(())
    } else {
        log(&"Fatal error!".red().bold());
        Err(eyre!("Git command error.")
            .with_suggestion(|| "Consider manually removing the `.checkpoint.error` file"))
    }
}

/// Persists the starting branch to a file so a later `savepoint finalize`
/// merges into the correct base branch.
fn create_statefile(starting_branch: &str) -> Result<()> {
    write(STATEFILE, starting_branch)?;
    Ok(())
}

/// Removes the statefile
fn rm_statefile() -> Result<()> {
    remove_file(STATEFILE)?;
    Ok(())
}

/// Read the statefile for target branch information
fn read_statefile() -> Result<String> {
    read_to_string(STATEFILE)
        .map(|s| s.trim().to_string())
        .map_err(|_| {
            eyre!("Could not read statefile")
                .with_suggestion(|| "Consider running 'savepoint start' first")
        })
}

fn create_errfile() -> Result<()> {
    write(ERRFILE, "")?;
    Ok(())
}

fn rm_errfile() -> Result<()> {
    remove_file(ERRFILE)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::*;

    use super::*;

    /// Helper: run `git <args>` in the current working directory and assert
    /// success.
    fn git(args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[rstest]
    #[case(State::Passing, "which", "which")]
    #[case(State::Failing, "which", "nonexistingbin12345")]
    #[timeout(Duration::from_secs(1))]
    // TODO: Refactor this
    fn app_test(#[case] state: State, #[case] program: &str, #[case] params: String) {
        let params = &[params];
        let app = SavePoint::new(program, params);
        let run = app.test(program, true, true);
        assert_eq!(run.expect("SavePoint::test returned an error").state, state);
    }

    #[test]
    #[serial_test::serial]
    fn is_git_repo_ok_inside_repo() {
        let tmp = tempfile::tempdir().expect("could not create tempdir!");
        std::process::Command::new("git")
            .arg("init")
            .current_dir(tmp.path())
            .output()
            .expect("failed to run `git init` in tempdir");

        let original = current_dir().expect("current dir did not return valid pathbuf val!");
        set_current_dir(tmp.path()).expect("failed to set current dir to tempdir");
        let result = is_git_repo();
        set_current_dir(&original).expect("failed to restore original current dir");

        assert!(result.is_ok());
    }

    #[test]
    #[serial_test::serial]
    fn is_git_repo_err_outside_repo() {
        let tmp = tempfile::tempdir().expect("could not create tempdir!");

        let original = current_dir().expect("current dir did not return valid pathbuf val!");
        set_current_dir(tmp.path()).expect("failed to set current dir to tempdir");
        let result = is_git_repo();
        set_current_dir(&original).expect("failed to restore original current dir");

        assert!(result.is_err());
    }

    #[rstest]
    #[serial_test::serial]
    fn merge_squashed_test() {
        let original_cwd = current_dir().expect("current dir did not return valid pathbuf val!");
        let temp = tempfile::TempDir::new().expect("could not create tempdir!");
        set_current_dir(temp.path()).expect("failed to set current dir to tempdir");

        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        write("file.txt", "v0").expect("failed to write file.txt v0");
        git(&["add", "-A"]);
        git(&["commit", "-qm", "initial"]);
        git(&["checkout", "-qb", "feat/foo"]);
        git(&["checkout", "-qb", "savepoint/test"]);
        write("file.txt", "v1").expect("failed to write file.txt v1");
        git(&["commit", "-qam", "savepoint commit"]);

        merge_squashed("feat/foo", Some("test merge".into()), false)
            .expect("merge_squashed failed");

        assert_eq!(
            current_branch().expect("Current branch should be feat/foo"),
            "feat/foo"
        );

        let log = std::process::Command::new("git")
            .args(["log", "--oneline"])
            .output()
            .expect("git log should be available");
        let log_str = String::from_utf8_lossy(&log.stdout);
        assert!(log_str.contains("test merge"), "log was: {log_str}");

        // Restore CWD before TempDir drops the dir.
        set_current_dir(&original_cwd).expect("Failed to restore CWD");
    }
}
